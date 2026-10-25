//! `tickloop monitor popup install|remove` against a private Herdr session, and the popup the
//! installed plugin opens: the same hosted Tick process, until Esc closes it.

use crate::support;

use std::{
    io::Write,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::Duration,
};

use portable_pty::{CommandBuilder, PtySize, native_pty_system};
use serde_json::{Value, json};
use support::herdr::{HerdrClient, PLUGIN_ID, PrivateHerdr, TIMEOUT, capture, wait_until};
use tickloop_code_local_environment::hosted::controller::HostController;
use uuid::Uuid;

const PLUGIN_DIRECTORY: &str = "herdr-popup-plugin";
const INSTALL_PROMPT: &str = "This registers Tickloop's popup plugin in the given Herdr session; no Herdr or Tick service is started, stopped or replaced. Install? [y/N]";
const PRESERVED_SETTINGS: &str = r#"{"prefersReducedMotion":true}"#;
const KEYS_TICK: &str = include_str!("../support/keys_tick.py");

/// The Tickloop state one test's commands use: settings, the generated plugin, and a home.
struct State {
    directory: tempfile::TempDir,
}

impl State {
    fn new() -> Self {
        let directory = tempfile::Builder::new()
            .prefix("tms")
            .tempdir_in(tickloop_harness::test_support::SHORT_TEMP_ROOT)
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
            "title": "Tickloop",
            "placement": "popup",
            "width": "85%",
            "height": "85%",
            "command": [executable, "monitor", "attach"],
        }]),
        "the popup runs the fixed attach program only"
    );
}

/// Only an explicit yes in a terminal links Tickloop's own plugin and turns `monitorPopup` on;
/// No, Enter, EOF and a non-interactive run change nothing. A same-id plugin Tickloop did not
/// write is never replaced or removed. Removing unlinks only Tickloop's plugin and turns the
/// setting off; other settings stay.
#[test]
fn popup_install_and_remove_change_only_tickloops_plugin_after_an_explicit_yes() {
    let herdr = PrivateHerdr::start();
    let state = State::new();

    let refused = state.run_without_terminal("install", &herdr);
    assert!(!refused.succeeded, "{refused:?}");
    assert!(
        refused.output.contains("interactive terminal"),
        "{refused:?}"
    );
    for answer in [&b"n\r"[..], b"\r", b"\x04", b"yess\r"] {
        let cancelled = state.run_in_terminal("install", &herdr, answer);
        assert!(!cancelled.succeeded, "{answer:?}: {cancelled:?}");
        assert!(cancelled.output.contains(INSTALL_PROMPT), "{cancelled:?}");
        assert!(cancelled.output.contains("cancelled"), "{cancelled:?}");
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
    assert!(kept.output.contains("cancelled"), "{kept:?}");
    assert_our_plugin(&herdr, &state);
    assert_eq!(state.settings_value()["monitorPopup"], "enabled");

    let removed = state.run_in_terminal("remove", &herdr, b"y\r");
    assert!(removed.succeeded, "{removed:?}");
    for fact in [PLUGIN_ID, "full screen", "running Ticks keep running"] {
        assert!(removed.output.contains(fact), "{fact}: {removed:?}");
    }
    assert!(herdr.plugins().is_empty());
    assert_eq!(
        state.settings_value(),
        json!({"prefersReducedMotion": true, "monitorPopup": "disabled"})
    );
}

/// A client of the private Herdr session in its own terminal, where the popup appears.
/// The `tickloop monitor attach` processes the private Herdr server runs for its popup.
fn popup_attach_processes(herdr: &PrivateHerdr) -> Vec<u32> {
    let attach = Path::new(env!("CARGO_BIN_EXE_tickloop"))
        .canonicalize()
        .unwrap();
    #[cfg(target_os = "macos")]
    let children = {
        const INITIAL_CHILD_PID_CAPACITY: usize = 16;
        let mut children = vec![0 as libc::pid_t; INITIAL_CHILD_PID_CAPACITY];
        loop {
            let count = unsafe {
                libc::proc_listchildpids(
                    herdr.server.id() as libc::pid_t,
                    children.as_mut_ptr().cast(),
                    std::mem::size_of_val(children.as_slice()) as libc::c_int,
                )
            };
            assert!(count >= 0, "{}", std::io::Error::last_os_error());
            if (count as usize) < children.len() {
                children.truncate(count as usize);
                break children
                    .into_iter()
                    .map(|pid| pid as u32)
                    .collect::<Vec<_>>();
            }
            children.resize(children.len() * 2, 0);
        }
    };
    #[cfg(target_os = "linux")]
    let children = {
        let mut children = std::fs::read_dir(format!("/proc/{}/task", herdr.server.id()))
            .unwrap()
            .filter_map(Result::ok)
            .filter_map(|thread| std::fs::read_to_string(thread.path().join("children")).ok())
            .flat_map(|pids| {
                pids.split_whitespace()
                    .filter_map(|pid| pid.parse::<u32>().ok())
                    .collect::<Vec<_>>()
            })
            .collect::<Vec<_>>();
        children.sort_unstable();
        children.dedup();
        children
    };
    children
        .into_iter()
        .filter(|pid| {
            #[cfg(target_os = "macos")]
            let executable = {
                use std::os::unix::ffi::OsStrExt;
                let mut path = vec![0; libc::PROC_PIDPATHINFO_MAXSIZE as usize];
                let length = unsafe {
                    libc::proc_pidpath(
                        *pid as libc::pid_t,
                        path.as_mut_ptr().cast(),
                        path.len() as u32,
                    )
                };
                (length > 0)
                    .then(|| PathBuf::from(std::ffi::OsStr::from_bytes(&path[..length as usize])))
            };
            #[cfg(target_os = "linux")]
            let executable = std::fs::read_link(format!("/proc/{pid}/exe")).ok();
            executable.as_ref() == Some(&attach)
        })
        .collect()
}

fn row(rows: &Value, opened: &tickhost::host::OpenedTick) -> Value {
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
/// process, a second popup is refused while the first stays, and a lone Esc closes the popup
/// while the Tick keeps running and can be shown again with its state.
#[tokio::test(flavor = "multi_thread")]
async fn the_installed_popup_shows_the_same_hosted_tick_until_esc_closes_it() {
    let herdr = PrivateHerdr::start();
    let state = State::new();
    let installed = state.run_in_terminal("install", &herdr, b"y\r");
    assert!(installed.succeeded, "{installed:?}");
    assert_our_plugin(&herdr, &state);
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
        .tempdir_in(tickloop_harness::test_support::SHORT_TEMP_ROOT)
        .unwrap();
    let controller = HostController::new(
        directory.path(),
        tickhost::paths::HostPaths::of_config_root(Some(directory.path())),
        Uuid::new_v4(),
        support::hosted::executable_in(directory.path()),
        support::hosted::IDLE_TIMEOUT,
    );
    let owner = tickhost::host::TickOwner {
        root: "fixture-root".into(),
        chat: Uuid::new_v4(),
    };
    let script = directory.path().join("keys.py");
    std::fs::write(&script, KEYS_TICK).unwrap();
    let opened = controller
        .open(tickhost::host::OpenRequest {
            operation: Uuid::new_v4(),
            owner: owner.clone(),
            owner_revision: 0,
            adapter: tickhost::host::TickAdapter::Protocol,
            command: vec![
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
    let popup = |grant: &tickloop_harness::protocol::HostedViewGrant| {
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
    assert!(shown.contains("┌Tickloop"), "{shown}");
    let listed = row(&controller.list(&owner, None).await.unwrap(), &opened);
    assert_eq!(listed["views"], 1, "{listed}");
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
    client.send(b"\x02\x1b");
    let first_kept = client.shows("keys 78 1b");
    let listed = row(&controller.list(&owner, None).await.unwrap(), &opened);
    assert_eq!(listed["views"], 1);
    assert_eq!(listed["business_pid"], opened.business_pid);
    assert_eq!(popup_attach_processes(&herdr), [first_attach]);

    client.send(b"\x1b");
    let mut detached = None;
    for _ in 0..(TIMEOUT.as_millis() / 25) {
        let listed = row(&controller.list(&owner, None).await.unwrap(), &opened);
        if listed["views"] == 0 {
            detached = Some(listed);
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    let detached = detached.expect("Esc releases the view");
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
    client.send(b"\x1b");
    wait_until("the reopened popup closes", || {
        Some(client.screen()).filter(|screen| !screen.contains("keys 78"))
    });
    wait_until("the second attach process exits", || {
        popup_attach_processes(&herdr).is_empty().then_some(())
    });
    eprintln!(
        "workspace {} business pid {} attach pids {first_attach} then {second_attach}\nfirst popup:\n{first_kept}\nafter Esc:\n{closed}\nreopened:\n{reopened}",
        workspace["workspace"]["workspace_id"], opened.business_pid
    );
    controller
        .close(Uuid::new_v4(), &owner, &opened.instance)
        .await
        .unwrap();
}

/// A popup whose attach client cannot show its Tick keeps the reason on screen until a key is
/// pressed instead of vanishing at once; that key closes it.
#[test]
fn a_popup_that_cannot_attach_shows_why_until_a_key_closes_it() {
    let herdr = PrivateHerdr::start();
    let state = State::new();
    let installed = state.run_in_terminal("install", &herdr, b"y\r");
    assert!(installed.succeeded, "{installed:?}");
    herdr
        .request(
            "workspace.create",
            json!({"cwd": herdr.root.path(), "label": "popup", "focus": true}),
        )
        .unwrap();
    let mut client = HerdrClient::attach(&herdr);
    client.shows("popup");

    let unreachable = tickloop_harness::protocol::HostedViewGrant {
        endpoint: herdr.root.path().join("no-tickhost.sock"),
        host_epoch: Uuid::new_v4(),
        instance: "hosted:00000000-0000-0000-0000-000000000000"
            .parse()
            .unwrap(),
        view: Uuid::new_v4(),
    };
    let opened = herdr.request(
        "plugin.pane.open",
        json!({
            "plugin_id": PLUGIN_ID,
            "entrypoint": "attach",
            "env": {"TICKLOOP_MONITOR_ATTACH": serde_json::to_string(&unreachable).unwrap()},
        }),
    );
    assert_eq!(opened.unwrap()["type"], "ok");
    let shown = client.shows("Press any key to close");
    assert!(shown.contains("┌Tickloop"), "{shown}");
    assert!(
        shown.contains("HostUnavailable"),
        "the reason is shown: {shown}"
    );
    std::thread::sleep(Duration::from_millis(500));
    assert!(
        client.screen().contains("Press any key to close"),
        "the popup waits for a key"
    );

    client.send(b"x");
    wait_until("the popup closes", || {
        Some(()).filter(|()| !client.screen().contains("┌Tickloop"))
    });
    wait_until("the attach process exits", || {
        popup_attach_processes(&herdr).is_empty().then_some(())
    });
}
