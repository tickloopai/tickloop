//! `tickloop monitor popup install|remove` against a private Herdr session, and the popup the
//! installed plugin opens: the same hosted Tick process, until Ctrl+B q closes it.

mod support;

use std::{
    io::{Read, Write},
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use portable_pty::{CommandBuilder, PtySize, native_pty_system};
use serde_json::{Value, json};
use tickloop_agent::hosted::controller::HostController;
use uuid::Uuid;

const TIMEOUT: Duration = Duration::from_secs(20);
const PLUGIN_ID: &str = "tickloop.monitor";
const PLUGIN_DIRECTORY: &str = "herdr-popup-plugin";
const INSTALL_PROMPT: &str = "这会在指定Herdr会话注册Tick Loop popup插件；不会启动、停止或替换Herdr/Tick服务。确认安装？[y/N]";
const PRESERVED_SETTINGS: &str = r#"{"prefersReducedMotion":true}"#;
const HERDR_CONFIG: &str = r#"onboarding = false
[terminal]
default_shell = "/bin/sh"
shell_mode = "non_login"
[update]
version_check = false
manifest_check = false
[ui.sound]
enabled = false
[ui.toast]
delivery = "off"
[ui.toast.clipboard]
enabled = false
[session]
resume_agents_on_restore = false
"#;
const HERDR_SESSION: &str = "p";
const KEYS_TICK: &str = include_str!("support/keys_tick.py");

fn wait_until<T>(what: &str, mut probe: impl FnMut() -> Option<T>) -> T {
    let deadline = Instant::now() + TIMEOUT;
    loop {
        if let Some(value) = probe() {
            return value;
        }
        assert!(Instant::now() < deadline, "{what} never happened");
        std::thread::sleep(Duration::from_millis(25));
    }
}

fn herdr_executable() -> PathBuf {
    std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
        .map(|directory| directory.join("herdr"))
        .find(|candidate| candidate.is_file())
        .expect("the popup tests run a private Herdr 0.9 server; put herdr on PATH")
}

/// A Herdr server of its own: private config, state, home and socket, stopped when dropped.
struct PrivateHerdr {
    root: tempfile::TempDir,
    server: Child,
}

impl PrivateHerdr {
    fn start() -> Self {
        let root = tempfile::Builder::new()
            .prefix("tmh")
            .tempdir_in("/private/tmp")
            .unwrap();
        std::fs::write(root.path().join("config.toml"), HERDR_CONFIG).unwrap();
        std::fs::create_dir(root.path().join("home")).unwrap();
        let log = std::fs::File::create(root.path().join("server.log")).unwrap();
        let server = Command::new(herdr_executable())
            .args(["--session", HERDR_SESSION, "server"])
            .env_clear()
            .envs(Self::environment(root.path()))
            .current_dir(root.path())
            .stdin(Stdio::null())
            .stdout(log.try_clone().unwrap())
            .stderr(log)
            .spawn()
            .unwrap();
        let herdr = Self { root, server };
        wait_until("the private Herdr socket", || {
            std::fs::symlink_metadata(herdr.socket())
                .ok()
                .filter(|_| herdr.request("ping", json!({})).is_ok())
        });
        eprintln!(
            "private Herdr server pid {} socket {} ping {}",
            herdr.server.id(),
            herdr.socket().display(),
            herdr.request("ping", json!({})).unwrap()
        );
        herdr
    }

    fn environment(root: &Path) -> Vec<(&'static str, PathBuf)> {
        vec![
            ("PATH", "/usr/bin:/bin".into()),
            ("HOME", root.join("home")),
            ("TERM", "xterm-256color".into()),
            ("SHELL", "/bin/sh".into()),
            ("XDG_CONFIG_HOME", root.to_path_buf()),
            ("XDG_STATE_HOME", root.join("state")),
            ("HERDR_CONFIG_PATH", root.join("config.toml")),
        ]
    }

    fn socket(&self) -> PathBuf {
        self.root
            .path()
            .join("herdr/sessions")
            .join(HERDR_SESSION)
            .join("herdr.sock")
    }

    /// One API request; `Err` carries Herdr's error body.
    fn request(&self, method: &str, params: Value) -> Result<Value, Value> {
        let mut stream = std::os::unix::net::UnixStream::connect(self.socket())
            .map_err(|error| json!({"code": "connect", "message": error.to_string()}))?;
        let request = json!({"id": Uuid::new_v4().to_string(), "method": method, "params": params});
        stream.write_all(format!("{request}\n").as_bytes()).unwrap();
        let mut response = Vec::new();
        let mut byte = [0];
        while stream.read(&mut byte).unwrap() == 1 && byte[0] != b'\n' {
            response.push(byte[0]);
        }
        let response: Value = serde_json::from_slice(&response).unwrap();
        match response.get("error") {
            Some(error) => Err(error.clone()),
            None => Ok(response["result"].clone()),
        }
    }

    fn plugins(&self) -> Vec<Value> {
        self.request("plugin.list", json!({"plugin_id": PLUGIN_ID}))
            .unwrap()["plugins"]
            .as_array()
            .unwrap()
            .clone()
    }

    /// Links a plugin Tick Loop did not write under Tick Loop's plugin id.
    fn link_foreign_plugin(&self) -> PathBuf {
        let directory = self.root.path().join("foreign-plugin");
        std::fs::create_dir_all(&directory).unwrap();
        std::fs::write(
            directory.join("herdr-plugin.toml"),
            format!(
                "id = \"{PLUGIN_ID}\"\nname = \"Someone else\"\nversion = \"1.0.0\"\nmin_herdr_version = \"0.9.0\"\n\n[[panes]]\nid = \"attach\"\ntitle = \"Other\"\nplacement = \"popup\"\ncommand = [\"/bin/sh\", \"-c\", \"exit 0\"]\n"
            ),
        )
        .unwrap();
        self.request(
            "plugin.link",
            json!({"path": directory.canonicalize().unwrap()}),
        )
        .unwrap();
        directory.canonicalize().unwrap()
    }
}

impl Drop for PrivateHerdr {
    fn drop(&mut self) {
        let _ = self.request("server.stop", json!({}));
        let deadline = Instant::now() + Duration::from_secs(8);
        while self.server.try_wait().unwrap().is_none() {
            if Instant::now() >= deadline {
                let _ = self.server.kill();
                let _ = self.server.wait();
                break;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        eprintln!("private Herdr server pid {} stopped", self.server.id());
    }
}

/// The Tick Loop state one test's commands use: settings, the generated plugin, and a home.
struct State {
    directory: tempfile::TempDir,
}

impl State {
    fn new() -> Self {
        let directory = tempfile::Builder::new()
            .prefix("tms")
            .tempdir_in("/private/tmp")
            .unwrap();
        std::fs::create_dir(directory.path().join("config")).unwrap();
        std::fs::create_dir(directory.path().join("home")).unwrap();
        std::fs::write(
            directory.path().join("config/settings.json"),
            PRESERVED_SETTINGS,
        )
        .unwrap();
        Self { directory }
    }

    fn config(&self) -> PathBuf {
        self.directory.path().join("config")
    }

    fn settings(&self) -> String {
        std::fs::read_to_string(self.config().join("settings.json")).unwrap()
    }

    fn settings_value(&self) -> Value {
        serde_json::from_str(&self.settings()).unwrap()
    }

    fn plugin_directory(&self) -> PathBuf {
        self.config().canonicalize().unwrap().join(PLUGIN_DIRECTORY)
    }

    fn command(&self, change: &str, herdr: &PrivateHerdr) -> Vec<String> {
        vec![
            "monitor".into(),
            "popup".into(),
            change.into(),
            "--herdr-socket".into(),
            herdr.socket().to_string_lossy().into_owned(),
        ]
    }

    /// Runs `tickloop monitor popup CHANGE` in a terminal and answers its question with `answer`.
    fn run_in_terminal(&self, change: &str, herdr: &PrivateHerdr, answer: &[u8]) -> Finished {
        let pty = native_pty_system()
            .openpty(PtySize {
                rows: 24,
                cols: 200,
                pixel_width: 0,
                pixel_height: 0,
            })
            .unwrap();
        let mut command = CommandBuilder::new(env!("CARGO_BIN_EXE_tickloop"));
        command.args(self.command(change, herdr));
        command.env_clear();
        command.env("PATH", "/usr/bin:/bin");
        command.env("HOME", self.directory.path().join("home"));
        command.env("TERM", "xterm-256color");
        command.env("TICKLOOP_CONFIG_DIR", self.config());
        let mut child = pty.slave.spawn_command(command).unwrap();
        drop(pty.slave);
        let output = capture(pty.master.try_clone_reader().unwrap());
        let mut writer = pty.master.take_writer().unwrap();
        let mut answered = false;
        let status = wait_until("the popup command exits", || {
            if !answered && String::from_utf8_lossy(&output.lock().unwrap()).contains("[y/N]") {
                writer.write_all(answer).unwrap();
                writer.flush().unwrap();
                answered = true;
            }
            child.try_wait().unwrap()
        });
        // The reader thread drains what the command wrote before it exited.
        std::thread::sleep(Duration::from_millis(100));
        Finished {
            succeeded: status.success(),
            output: String::from_utf8_lossy(&output.lock().unwrap()).into_owned(),
        }
    }

    fn run_without_terminal(&self, change: &str, herdr: &PrivateHerdr) -> Finished {
        let output = Command::new(env!("CARGO_BIN_EXE_tickloop"))
            .args(self.command(change, herdr))
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", self.directory.path().join("home"))
            .env("TICKLOOP_CONFIG_DIR", self.config())
            .stdin(Stdio::null())
            .output()
            .unwrap();
        Finished {
            succeeded: output.status.success(),
            output: format!(
                "{}{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            ),
        }
    }
}

#[derive(Debug)]
struct Finished {
    succeeded: bool,
    output: String,
}

fn capture(mut reader: Box<dyn Read + Send>) -> Arc<Mutex<Vec<u8>>> {
    let output = Arc::new(Mutex::new(Vec::new()));
    let captured = output.clone();
    std::thread::spawn(move || {
        let mut chunk = [0; 8192];
        while let Ok(count) = reader.read(&mut chunk) {
            if count == 0 {
                break;
            }
            captured.lock().unwrap().extend_from_slice(&chunk[..count]);
        }
    });
    output
}

fn assert_our_plugin(herdr: &PrivateHerdr, state: &State) {
    let plugins = herdr.plugins();
    assert_eq!(plugins.len(), 1, "{plugins:?}");
    let plugin = &plugins[0];
    assert_eq!(
        plugin["plugin_root"],
        json!(state.plugin_directory()),
        "{plugin}"
    );
    assert_eq!(plugin["enabled"], json!(true));
    let executable = Path::new(env!("CARGO_BIN_EXE_tickloop"))
        .canonicalize()
        .unwrap();
    assert_eq!(
        plugin["panes"],
        json!([{
            "id": "attach",
            "title": "Tick Loop",
            "placement": "popup",
            "width": "85%",
            "height": "85%",
            "command": [executable, "monitor", "attach"],
        }]),
        "the popup runs the fixed attach program only"
    );
}

/// Only an explicit yes in a terminal links Tick Loop's own plugin and turns `monitorPopup` on;
/// No, Enter, EOF and a non-interactive run change nothing. A same-id plugin Tick Loop did not
/// write is never replaced or removed. Removing unlinks only Tick Loop's plugin and turns the
/// setting off; other settings stay.
#[test]
fn popup_install_and_remove_change_only_tick_loops_plugin_after_an_explicit_yes() {
    let herdr = PrivateHerdr::start();
    let state = State::new();

    let refused = state.run_without_terminal("install", &herdr);
    assert!(!refused.succeeded, "{refused:?}");
    assert!(refused.output.contains("交互终端"), "{refused:?}");
    for answer in [&b"n\r"[..], b"\r", b"\x04", b"yess\r"] {
        let cancelled = state.run_in_terminal("install", &herdr, answer);
        assert!(!cancelled.succeeded, "{answer:?}: {cancelled:?}");
        assert!(cancelled.output.contains(INSTALL_PROMPT), "{cancelled:?}");
        assert!(cancelled.output.contains("已取消"), "{cancelled:?}");
        assert!(herdr.plugins().is_empty(), "{answer:?} links nothing");
        assert_eq!(state.settings(), PRESERVED_SETTINGS, "{answer:?}");
        assert!(
            !state.config().join(PLUGIN_DIRECTORY).exists(),
            "{answer:?} writes no plugin"
        );
    }

    let foreign = herdr.link_foreign_plugin();
    for change in ["install", "remove"] {
        let refused = state.run_in_terminal(change, &herdr, b"y\r");
        assert!(!refused.succeeded, "{change}: {refused:?}");
        assert!(!refused.output.contains("[y/N]"), "{change} asks nothing");
        assert!(refused.output.contains(PLUGIN_ID), "{refused:?}");
        let plugins = herdr.plugins();
        assert_eq!(plugins.len(), 1);
        assert_eq!(
            plugins[0]["plugin_root"],
            json!(foreign),
            "{change} keeps it"
        );
        assert_eq!(state.settings(), PRESERVED_SETTINGS);
    }
    herdr
        .request("plugin.unlink", json!({"plugin_id": PLUGIN_ID}))
        .unwrap();
    let absent = state.run_in_terminal("remove", &herdr, b"y\r");
    assert!(!absent.succeeded, "nothing to remove: {absent:?}");
    assert_eq!(state.settings(), PRESERVED_SETTINGS);

    let installed = state.run_in_terminal("install", &herdr, b"y\r");
    assert!(installed.succeeded, "{installed:?}");
    assert!(installed.output.contains(INSTALL_PROMPT), "{installed:?}");
    assert_our_plugin(&herdr, &state);
    assert_eq!(
        state.settings_value(),
        json!({"prefersReducedMotion": true, "monitorPopup": "enabled"})
    );
    let directory = std::fs::metadata(state.plugin_directory()).unwrap();
    assert_eq!(directory.permissions().mode() & 0o777, 0o700);
    let manifest = std::fs::metadata(state.plugin_directory().join("herdr-plugin.toml")).unwrap();
    assert_eq!(manifest.permissions().mode() & 0o777, 0o600);

    let reinstalled = state.run_in_terminal("install", &herdr, b"yes\r");
    assert!(
        reinstalled.succeeded,
        "its own plugin is refreshed: {reinstalled:?}"
    );
    assert_our_plugin(&herdr, &state);

    let kept = state.run_in_terminal("remove", &herdr, b"n\r");
    assert!(!kept.succeeded, "{kept:?}");
    assert!(kept.output.contains("[y/N]"), "{kept:?}");
    assert!(kept.output.contains("已取消"), "{kept:?}");
    assert_our_plugin(&herdr, &state);
    assert_eq!(state.settings_value()["monitorPopup"], "enabled");

    let removed = state.run_in_terminal("remove", &herdr, b"y\r");
    assert!(removed.succeeded, "{removed:?}");
    for fact in [PLUGIN_ID, "snapshot", "Tick继续运行"] {
        assert!(removed.output.contains(fact), "{fact}: {removed:?}");
    }
    assert!(herdr.plugins().is_empty());
    assert_eq!(
        state.settings_value(),
        json!({"prefersReducedMotion": true, "monitorPopup": "disabled"})
    );
}

/// A client of the private Herdr session in its own terminal, where the popup appears.
struct HerdrClient {
    child: Box<dyn portable_pty::Child + Send + Sync>,
    writer: Box<dyn Write + Send>,
    output: Arc<Mutex<Vec<u8>>>,
}

const CLIENT_ROWS: u16 = 44;
const CLIENT_COLUMNS: u16 = 140;

impl HerdrClient {
    fn attach(herdr: &PrivateHerdr) -> Self {
        let pty = native_pty_system()
            .openpty(PtySize {
                rows: CLIENT_ROWS,
                cols: CLIENT_COLUMNS,
                pixel_width: 0,
                pixel_height: 0,
            })
            .unwrap();
        let mut command = CommandBuilder::new(herdr_executable());
        command.args(["--session", HERDR_SESSION]);
        command.env_clear();
        for (name, value) in PrivateHerdr::environment(herdr.root.path()) {
            command.env(name, value);
        }
        command.cwd(herdr.root.path());
        let child = pty.slave.spawn_command(command).unwrap();
        drop(pty.slave);
        let output = capture(pty.master.try_clone_reader().unwrap());
        Self {
            writer: pty.master.take_writer().unwrap(),
            child,
            output,
        }
    }

    fn screen(&self) -> String {
        let mut parser = vt100::Parser::new(CLIENT_ROWS, CLIENT_COLUMNS, 0);
        parser.process(&self.output.lock().unwrap());
        parser.screen().contents()
    }

    fn shows(&self, text: &str) -> String {
        wait_until(&format!("{text:?} on the Herdr client"), || {
            Some(self.screen()).filter(|screen| screen.contains(text))
        })
    }

    fn send(&mut self, bytes: &[u8]) {
        self.writer.write_all(bytes).unwrap();
        self.writer.flush().unwrap();
    }
}

impl Drop for HerdrClient {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// The `tickloop monitor attach` processes the private Herdr server runs for its popup.
fn popup_attach_processes(herdr: &PrivateHerdr) -> Vec<u32> {
    let attach = format!(
        "{} monitor attach",
        Path::new(env!("CARGO_BIN_EXE_tickloop"))
            .canonicalize()
            .unwrap()
            .display()
    );
    let listed = Command::new("/bin/ps")
        .args(["-ax", "-o", "pid=,ppid=,command="])
        .output()
        .unwrap();
    String::from_utf8_lossy(&listed.stdout)
        .lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            let pid = fields.next()?.parse().ok()?;
            let parent = fields.next()?.parse::<u32>().ok()?;
            let command = fields.collect::<Vec<_>>().join(" ");
            (parent == herdr.server.id() && command == attach).then_some(pid)
        })
        .collect()
}

fn row(rows: &Value, opened: &tick_host::host::OpenedTick) -> Value {
    rows["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["instance"] == json!(opened.instance))
        .cloned()
        .expect("the hosted Tick is listed")
}

/// The installed plugin's popup, opened directly on the private server as the fixture allows,
/// attaches to the hosted Tick's own terminal: its screen renders, keys reach the same business
/// process, a second popup is refused while the first stays, and Ctrl+B q closes the popup
/// while the Tick keeps running and can be shown again with its state.
#[tokio::test(flavor = "multi_thread")]
async fn the_installed_popup_shows_the_same_hosted_tick_until_it_detaches() {
    let herdr = PrivateHerdr::start();
    let state = State::new();
    let installed = state.run_in_terminal("install", &herdr, b"y\r");
    assert!(installed.succeeded, "{installed:?}");
    let workspace = herdr
        .request(
            "workspace.create",
            json!({"cwd": herdr.root.path(), "label": "popup", "focus": true}),
        )
        .unwrap();
    let mut client = HerdrClient::attach(&herdr);
    client.shows("popup");

    let directory = tempfile::Builder::new()
        .prefix("tmp")
        .tempdir_in("/private/tmp")
        .unwrap();
    let controller = HostController::new(
        directory.path(),
        tick_host::paths::HostPaths {
            runtime: directory.path().join("h"),
        },
        Uuid::new_v4(),
        support::fixture_cli::executable(),
        tick_host::host::DEFAULT_IDLE_TIMEOUT,
    );
    let owner = tick_host::host::TickOwner {
        root: "fixture-root".into(),
        chat: Uuid::new_v4(),
    };
    let script = directory.path().join("keys.py");
    std::fs::write(&script, KEYS_TICK).unwrap();
    let opened = controller
        .open(tick_host::host::OpenRequest {
            operation: Uuid::new_v4(),
            owner: owner.clone(),
            owner_revision: 0,
            command: vec![
                support::fixture_cli::executable()
                    .to_string_lossy()
                    .into_owned(),
                "--tick-launch".into(),
                "--".into(),
                "/usr/bin/python3".into(),
                "-u".into(),
                script.to_string_lossy().into_owned(),
            ],
            cwd: directory.path().to_path_buf(),
            environment: vec![("PATH".into(), "/usr/bin:/bin".into())],
            title: None,
        })
        .await
        .unwrap();
    let popup = |grant: &tickloop_agent::hosted::controller::HostedViewGrant| {
        herdr.request(
            "plugin.pane.open",
            json!({
                "plugin_id": PLUGIN_ID,
                "entrypoint": "attach",
                "env": {"TICKLOOP_MONITOR_ATTACH": serde_json::to_string(grant).unwrap()},
            }),
        )
    };

    let grant = controller
        .allow_view(&owner, &opened.instance)
        .await
        .unwrap();
    assert_eq!(popup(&grant).unwrap()["type"], "ok");
    let shown = client.shows("keys");
    assert!(shown.contains("┌Tick Loop"), "{shown}");
    let listed = row(&controller.list(&owner, None).await.unwrap(), &opened);
    assert_eq!(listed["attached"], true, "{listed}");
    assert_eq!(listed["business_pid"], opened.business_pid);
    let first_attach = wait_until(
        "the popup's attach process",
        || match popup_attach_processes(&herdr)[..] {
            [pid] => Some(pid),
            _ => None,
        },
    );
    client.send(b"x");
    client.shows("keys 78");

    let second = controller
        .allow_view(&owner, &opened.instance)
        .await
        .unwrap();
    assert_eq!(popup(&second).unwrap_err()["code"], "ui_busy");
    client.send(b"\x1b");
    let first_kept = client.shows("keys 78 1b");
    let listed = row(&controller.list(&owner, None).await.unwrap(), &opened);
    assert_eq!(listed["attached"], true);
    assert_eq!(listed["business_pid"], opened.business_pid);
    assert_eq!(popup_attach_processes(&herdr), [first_attach]);

    client.send(b"\x02");
    std::thread::sleep(Duration::from_millis(200));
    client.send(b"q");
    let mut detached = None;
    for _ in 0..(TIMEOUT.as_millis() / 25) {
        let listed = row(&controller.list(&owner, None).await.unwrap(), &opened);
        if listed["attached"] == false {
            detached = Some(listed);
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    let detached = detached.expect("Ctrl+B q releases the view");
    assert_eq!(detached["business_pid"], opened.business_pid);
    let closed = wait_until("the popup closes", || {
        Some(client.screen()).filter(|screen| !screen.contains("keys 78"))
    });
    wait_until("the first attach process exits", || {
        popup_attach_processes(&herdr).is_empty().then_some(())
    });
    assert_eq!(
        unsafe { libc::kill(opened.business_pid as i32, 0) },
        0,
        "the Tick keeps running"
    );

    let again = controller
        .allow_view(&owner, &opened.instance)
        .await
        .unwrap();
    assert_eq!(popup(&again).unwrap()["type"], "ok", "the popup was closed");
    let reopened = client.shows("keys 78 1b");
    let second_attach =
        wait_until(
            "the reopened popup's attach process",
            || match popup_attach_processes(&herdr)[..] {
                [pid] => Some(pid),
                _ => None,
            },
        );
    assert_ne!(second_attach, first_attach);
    let listed = row(&controller.list(&owner, None).await.unwrap(), &opened);
    assert_eq!(listed["business_pid"], opened.business_pid);
    client.send(b"\x02q");
    wait_until("the reopened popup closes", || {
        Some(client.screen()).filter(|screen| !screen.contains("keys 78"))
    });
    wait_until("the second attach process exits", || {
        popup_attach_processes(&herdr).is_empty().then_some(())
    });
    eprintln!(
        "workspace {} business pid {} attach pids {first_attach} then {second_attach}\nfirst popup:\n{first_kept}\nafter Ctrl+B q:\n{closed}\nreopened:\n{reopened}",
        workspace["workspace"]["workspace_id"], opened.business_pid
    );
    controller
        .close(Uuid::new_v4(), &owner, &opened.instance)
        .await
        .unwrap();
}
