//! `tickloop tick` as a user runs it, against a private config root, with a local file or a
//! local HTTP server standing in for the Tick marketplace and its downloads.

use std::{
    collections::{BTreeMap, HashMap},
    fs,
    io::{BufRead, BufReader, Write},
    net::{Shutdown, TcpListener, TcpStream},
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
    sync::{Arc, Mutex},
    thread,
};

use flate2::{Compression, write::GzEncoder};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tick_packages::CURRENT_TARGET;

use crate::support;

const OTHER_TARGET: &str = "x86_64-unknown-linux-gnu";

struct Fixture {
    _directory: tempfile::TempDir,
    root: PathBuf,
    runtime: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        fs::create_dir(root.join("home")).unwrap();
        Self {
            _directory: directory,
            root,
            runtime: support::cli_runtime::directory(),
        }
    }

    fn tickloop(&self, arguments: &[&str]) -> Output {
        self.command(arguments).output().unwrap()
    }

    /// Runs tickloop with the Tick marketplace at `marketplace`, a local file or a URL.
    fn tickloop_from(&self, marketplace: &str, arguments: &[&str]) -> Output {
        self.command(arguments)
            .env("TICKLOOP_TICK_MARKETPLACE", marketplace)
            .output()
            .unwrap()
    }

    fn command(&self, arguments: &[&str]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_tickloop"));
        command
            .args(arguments)
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", self.root.join("home"))
            .env("NO_COLOR", "1")
            .env("TICKLOOP_CONFIG_DIR", self.root.join("config"))
            .env("TICKLOOP_DAEMON_RUNTIME_DIR", &self.runtime)
            .current_dir(&self.root)
            .stdin(Stdio::null());
        command
    }

    /// A package directory of `sample` at `version`, whose program prints `version`.
    fn package(&self, version: &str) -> PathBuf {
        self.package_named("sample", version)
    }

    fn package_named(&self, name: &str, version: &str) -> PathBuf {
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
                "credentials": format!("~/.tickloop/{name}/"),
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

    /// The package `sample` at `version` as a release archive, and its SHA-256.
    fn archive(&self, version: &str) -> (Vec<u8>, String) {
        let mut builder = tar::Builder::new(GzEncoder::new(Vec::new(), Compression::default()));
        builder.append_dir_all(".", self.package(version)).unwrap();
        let archive = builder.into_inner().unwrap().finish().unwrap();
        let sha256 = format!("{:x}", Sha256::digest(&archive));
        (archive, sha256)
    }

    /// Writes `marketplace` as the local marketplace file `label` and returns its path.
    fn marketplace_file(&self, label: &str, marketplace: &Value) -> String {
        self.text_file(label, &marketplace.to_string())
    }

    fn text_file(&self, label: &str, contents: &str) -> String {
        let path = self.root.join(format!("{label}.json"));
        fs::write(&path, contents).unwrap();
        path_text(&path)
    }

    fn installed_root(&self) -> PathBuf {
        self.root.join("config/ticks/sample")
    }

    fn packages_directory(&self) -> PathBuf {
        self.root.join("config/ticks")
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

fn marketplace_json(packages: Value) -> Value {
    json!({
        "name": "tickloop",
        "owner": { "name": "Tickloop AI", "url": "https://tickloop.ai" },
        "packages": packages,
    })
}

/// The marketplace entry of `name` at `version`, downloadable for each of `downloads`' targets.
fn offered(name: &str, version: &str, downloads: &[(&str, &str, &str)]) -> Value {
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
enum Reply {
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
struct Server {
    origin: String,
    replies: Arc<Mutex<HashMap<String, Reply>>>,
    requests: Arc<Mutex<Vec<String>>>,
}

impl Server {
    fn start() -> Self {
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
    fn serve(&self, path: &str, reply: Reply) -> String {
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

fn succeeded(output: &Output) -> String {
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

fn failed(output: &Output) -> String {
    assert_eq!(
        output.status.code(),
        Some(1),
        "stdout: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    String::from_utf8(output.stderr.clone()).unwrap()
}

fn path_text(path: &Path) -> String {
    path.display().to_string()
}

#[test]
fn a_package_installs_lists_updates_and_uninstalls_through_the_cli() {
    let fixture = Fixture::new();
    assert_eq!(
        succeeded(&fixture.tickloop(&["tick", "list"])),
        "No Tick packages are installed.\n"
    );
    let first = fixture.package("1.0.0");
    let first_path = path_text(&first);
    let installed_root = path_text(&fixture.installed_root());

    assert_eq!(
        succeeded(&fixture.tickloop(&["tick", "install", "--path", &first_path])),
        format!("Installed Tick package sample 1.0.0 in {installed_root}.\n")
    );
    let program = Command::new(fixture.installed_root().join("bin/tick-sample"))
        .output()
        .unwrap();
    assert_eq!(String::from_utf8(program.stdout).unwrap(), "sample 1.0.0\n");
    assert_eq!(
        succeeded(&fixture.tickloop(&["tick", "install", "--path", &first_path])),
        format!("Reinstalled Tick package sample 1.0.0 in {installed_root}.\n")
    );
    assert_eq!(
        succeeded(&fixture.tickloop(&["tick", "list"])),
        "sample  1.0.0  The sample package\n"
    );

    let second = path_text(&fixture.package("2.0.0"));
    assert_eq!(
        succeeded(&fixture.tickloop(&["tick", "install", "--path", &second])),
        format!("Replaced Tick package sample 1.0.0 with 2.0.0 in {installed_root}.\n")
    );
    assert_eq!(
        succeeded(&fixture.tickloop(&["tick", "list"])),
        "sample  2.0.0  The sample package\n"
    );

    assert_eq!(
        succeeded(&fixture.tickloop(&["tick", "uninstall", "sample"])),
        "Uninstalled Tick package sample 2.0.0.\n\
         Its data source credentials, if any, stay in ~/.tickloop/sample/.\n"
    );
    assert!(!fixture.installed_root().exists());
    assert_eq!(
        failed(&fixture.tickloop(&["tick", "uninstall", "sample"])),
        "Error: Tick package sample is not installed\n"
    );
    assert_eq!(
        succeeded(&fixture.tickloop(&["tick", "list"])),
        "No Tick packages are installed.\n"
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
        "sample  1.0.0  The sample package\n"
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
        "sample  1.0.0  unusable: executable bin/tick-sample does not exist in the package; \
         install it again\n"
    );

    let registry = fixture.root.join("config/ticks/installed.json");
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
        &["tick", "remove", "sample"],
    ] {
        let error = failed(&fixture.tickloop(arguments));
        assert!(
            error.starts_with("Error: Unknown tick command. Usage: tickloop tick install PACKAGE"),
            "{arguments:?}: {error}"
        );
    }
    for arguments in [
        &["tick", "--help"][..],
        &["tick", "-h"],
        &["tick", "install", "--help"],
        &["tick", "list", "-h"],
    ] {
        let help = succeeded(&fixture.tickloop(arguments));
        assert!(
            help.starts_with(
                "Usage: tickloop tick install PACKAGE\n       tickloop tick install --path PATH\n"
            ),
            "{help}"
        );
        assert!(help.contains("TICKLOOP_CONFIG_DIR"), "{help}");
        assert!(help.contains("TICKLOOP_TICK_MARKETPLACE"), "{help}");
        assert!(
            help.contains("grants no market data, account or trading access"),
            "{help}"
        );
    }
    let console_help = succeeded(&fixture.tickloop(&["--help"]));
    assert!(
        console_help.contains(
            "tickloop tick install PACKAGE | install --path PATH | uninstall PACKAGE | list [--available]"
        ),
        "{console_help}"
    );
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
    let installed_root = path_text(&fixture.installed_root());

    assert_eq!(
        succeeded(&fixture.tickloop_from(&marketplace, &["tick", "install", "sample"])),
        format!("Installed Tick package sample 1.0.0 in {installed_root}.\n")
    );
    let program = Command::new(fixture.installed_root().join("bin/tick-sample"))
        .output()
        .unwrap();
    assert_eq!(String::from_utf8(program.stdout).unwrap(), "sample 1.0.0\n");
    let registry: Value = serde_json::from_slice(
        &fs::read(fixture.packages_directory().join("installed.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(registry["packages"]["sample"]["source"], first_url);
    assert_eq!(registry["packages"]["sample"]["sha256"], first_sha256);

    assert_eq!(
        succeeded(&fixture.tickloop_from(&marketplace, &["tick", "install", "sample"])),
        format!(
            "Tick package sample 1.0.0 is already installed in {installed_root}; nothing changed.\n"
        )
    );
    assert_eq!(server.requests_of("/tick-sample-1.0.0.tar.gz"), 1);

    fs::remove_file(fixture.installed_root().join("bin/tick-sample")).unwrap();
    assert_eq!(
        succeeded(&fixture.tickloop_from(&marketplace, &["tick", "install", "sample"])),
        format!("Reinstalled Tick package sample 1.0.0 in {installed_root}.\n")
    );
    assert_eq!(server.requests_of("/tick-sample-1.0.0.tar.gz"), 2);
    assert!(fixture.installed_root().join("bin/tick-sample").is_file());

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
    assert_eq!(
        succeeded(&fixture.tickloop_from(&marketplace, &["tick", "install", "sample"])),
        format!("Replaced Tick package sample 1.0.0 with 2.0.0 in {installed_root}.\n")
    );
    assert_eq!(
        succeeded(&fixture.tickloop(&["tick", "list"])),
        "sample  2.0.0  The sample package\n"
    );
    fixture.assert_no_leftovers();
}

#[test]
fn a_failed_marketplace_install_says_why_and_changes_nothing() {
    let fixture = Fixture::new();
    let server = Server::start();
    succeeded(&fixture.tickloop(&[
        "tick",
        "install",
        "--path",
        &path_text(&fixture.package("1.0.0")),
    ]));
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
        let error = failed(&fixture.tickloop_from(&marketplace, &["tick", "install", "sample"]));
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
        "sample  1.0.0  The sample package\n"
    );

    let registry = fixture.packages_directory().join("installed.json");
    fs::write(&registry, "{").unwrap();
    let unreached_url = server.serve("/unreached.tar.gz", Reply::Body(Vec::new()));
    let error = failed(&fixture.tickloop_from(
        &offering("unreached", CURRENT_TARGET, &unreached_url, &sha256),
        &["tick", "install", "sample"],
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
    for (name, version) in [("current", "1.0.0"), ("outdated", "1.0.0")] {
        succeeded(&fixture.tickloop(&[
            "tick",
            "install",
            "--path",
            &path_text(&fixture.package_named(name, version)),
        ]));
    }
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

    let listing =
        succeeded(&fixture.tickloop_from(&marketplace_url, &["tick", "list", "--available"]));

    let mut lines = listing.lines();
    assert_eq!(
        lines.next().unwrap(),
        format!("Tick marketplace tickloop by Tickloop AI: {marketplace_url}")
    );
    let rows = lines
        .map(|line| {
            line.split("  ")
                .map(str::trim)
                .filter(|column| !column.is_empty())
                .collect::<Vec<_>>()
        })
        .collect::<Vec<_>>();
    let unavailable = format!("not installed; no prebuilt program for {CURRENT_TARGET}");
    assert_eq!(
        rows,
        [
            vec!["current", "1.0.0", "installed", "The current package"],
            vec!["elsewhere", "0.1.0", &unavailable, "The elsewhere package"],
            vec!["fresh", "0.3.0", "not installed", "The fresh package"],
            vec![
                "outdated",
                "2.0.0",
                "installed 1.0.0",
                "The outdated package"
            ],
        ]
    );

    let empty = fixture.marketplace_file("empty", &marketplace_json(json!([])));
    assert_eq!(
        succeeded(&fixture.tickloop_from(&empty, &["tick", "list", "--available"])),
        format!(
            "Tick marketplace tickloop by Tickloop AI: {empty}\nIt offers no Tick packages yet.\n"
        )
    );
    assert_eq!(
        failed(&fixture.tickloop_from(&empty, &["tick", "install", "fresh"])),
        format!("Error: Tick package fresh is not in the Tick marketplace at {empty}\n")
    );
}
