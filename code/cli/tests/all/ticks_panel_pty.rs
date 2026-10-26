//! `/ticks` and `/plugins` in the Agent's terminal, against a private config root, with a local marketplace file
//! and a local HTTP server for its downloads.

use std::{
    fs,
    io::Write,
    os::unix::fs::PermissionsExt,
    path::Path,
    sync::{Arc, Mutex},
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use portable_pty::{CommandBuilder, PtySize, native_pty_system};
use serde_json::json;
use tick_oauth_test_server::{AuthorizationServer, TokenReply, USER_CODE};
use tick_packages::{CURRENT_TARGET, OAuthToken, PackageCredentials};
use tickloop_harness::test_support::{input_dialog, screen};

use crate::tick_package_commands::{
    Fixture, Reply, Server, archive_of, marketplace_json, oauth2, offered, path_text, succeeded,
};

const COLUMNS: u16 = 110;
const ROWS: u16 = 36;
const TIMEOUT: Duration = Duration::from_secs(20);
const FRAME_SETTLE: Duration = Duration::from_millis(40);
const CONTROL_N: &[u8] = b"\x0e";
const CONTROL_P: &[u8] = b"\x10";
pub(crate) const ESCAPE: &[u8] = b"\x1b";
const TAB: &[u8] = b"\t";
pub(crate) struct Terminal {
    child: Box<dyn portable_pty::Child + Send + Sync>,
    writer: Box<dyn Write + Send>,
    output: Arc<Mutex<String>>,
    _master: Box<dyn portable_pty::MasterPty + Send>,
    fixture_home: std::path::PathBuf,
    config_root: std::path::PathBuf,
    columns: u16,
    rows: u16,
}

impl Terminal {
    pub(crate) fn start(fixture: &Fixture, marketplace: &str) -> Self {
        let root = &fixture.root;
        fs::create_dir_all(root.join("tmp")).unwrap();
        let pty = native_pty_system()
            .openpty(PtySize {
                rows: ROWS,
                cols: COLUMNS,
                pixel_width: 0,
                pixel_height: 0,
            })
            .unwrap();
        crate::support::cli_runtime::configure_model(&root.join("config"));
        let mut command = CommandBuilder::new(env!("CARGO_BIN_EXE_tickloop"));
        command.args([
            "--working-directory-is-trusted",
            "--mode",
            "acceptEdits",
            "--cwd",
        ]);
        command.arg(root);
        command.env("HOME", root.join("home"));
        command.env("TICKLOOP_TMPDIR", root.join("tmp"));
        command.env("TICKLOOP_CONFIG_DIR", root.join("config"));
        command.env("TICKLOOP_TICK_MARKETPLACE", marketplace);
        command.env("TICKLOOP_AGENT_TUI", "inline");
        command.env("TERM", "xterm-256color");
        let editor = root.join("filter-editor");
        if editor.exists() {
            command.env("VISUAL", editor);
        }
        command.env("NO_PROXY", "127.0.0.1,localhost");
        command.env("no_proxy", "127.0.0.1,localhost");
        for name in ["HERDR_ENV", "HERDR_PANE_ID", "HERDR_SOCKET_PATH", "TMUX"] {
            command.env_remove(name);
        }
        for model in tickloop_harness::model::cards::built_in_model_profiles() {
            if !model.api_key_env.is_empty() {
                command.env_remove(model.api_key_env);
            }
        }
        let child = pty.slave.spawn_command(command).unwrap();
        drop(pty.slave);
        let reader = pty.master.try_clone_reader().unwrap();
        let writer = pty.master.take_writer().unwrap();
        let output = Arc::new(Mutex::new(String::new()));
        let captured = output.clone();
        thread::spawn(move || screen::capture_output(reader, captured));
        Self {
            child,
            writer,
            output,
            _master: pty.master,
            fixture_home: root.join("home"),
            config_root: root.join("config"),
            columns: COLUMNS,
            rows: ROWS,
        }
    }

    fn resize(&mut self, columns: u16, rows: u16) {
        self.columns = columns;
        self.rows = rows;
        self._master
            .resize(PtySize {
                rows,
                cols: columns,
                pixel_width: 0,
                pixel_height: 0,
            })
            .unwrap();
    }

    pub(crate) fn send(&mut self, input: &[u8]) {
        self.writer.write_all(input).unwrap();
        self.writer.flush().unwrap();
    }

    pub(crate) fn mark(&self) -> usize {
        self.output.lock().unwrap().len()
    }

    pub(crate) fn frame(&self) -> String {
        screen::visible_screen_with_selection(
            &self.output.lock().unwrap(),
            usize::from(self.columns),
            usize::from(self.rows),
        )
    }

    /// Waits for a frame drawn after `mark` that `matches`.
    pub(crate) fn wait_for_frame(
        &self,
        mark: usize,
        label: &str,
        matches: impl Fn(&str) -> bool,
    ) -> String {
        let deadline = Instant::now() + TIMEOUT;
        while Instant::now() < deadline {
            let drawn = self.output.lock().unwrap()[mark..].contains("\x1b[?2026l");
            let frame = self.frame();
            if drawn && matches(&frame) {
                thread::sleep(FRAME_SETTLE);
                return self.frame();
            }
            thread::sleep(Duration::from_millis(10));
        }
        panic!(
            "terminal did not show {label}; latest frame:\n{}",
            self.frame()
        );
    }

    /// Sends `input` and waits for the frame it leads to.
    pub(crate) fn press(
        &mut self,
        input: &[u8],
        label: &str,
        matches: impl Fn(&str) -> bool,
    ) -> String {
        let mark = self.mark();
        self.send(input);
        self.wait_for_frame(mark, label, matches)
    }

    fn assert_alive(&mut self) {
        assert!(
            self.child.try_wait().unwrap().is_none(),
            "the Agent's terminal ended; latest frame:\n{}",
            self.frame()
        );
    }

    pub(crate) fn exit(&mut self) {
        self.send(b"/exit\r");
        let deadline = Instant::now() + TIMEOUT;
        while self.child.try_wait().unwrap().is_none() {
            assert!(
                Instant::now() < deadline,
                "terminal did not exit; latest frame:\n{}",
                self.frame()
            );
            thread::sleep(Duration::from_millis(10));
        }
    }
}

impl Drop for Terminal {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::process::Command::new(env!("CARGO_BIN_EXE_tickloop"))
            .args(["daemon", "stop"])
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("HOME", &self.fixture_home)
            .env("TICKLOOP_CONFIG_DIR", &self.config_root)
            .output();
    }
}

/// Saves a login for the package `name` as `tickloop tick login` would, one its server cannot
/// revoke, so logging out reaches no server.
fn save_login(fixture: &Fixture, name: &str) {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    PackageCredentials::in_config_root(&fixture.root.join("config"), name)
        .unwrap()
        .lock_oauth()
        .unwrap()
        .save_token(&OAuthToken {
            access_token: "access".into(),
            token_type: "Bearer".into(),
            refresh_token: Some("refresh".into()),
            scope: None,
            issuer: "https://auth.example.com".into(),
            client_id: "client".into(),
            token_endpoint: "https://auth.example.com/token".into(),
            revocation_endpoint: None,
            expires_at: now + 3600,
        })
        .unwrap();
}

fn install_from_path(fixture: &Fixture, command: &str, directory: &Path) {
    succeeded(&fixture.tickloop(&[command, "install", "--path", &path_text(directory)]));
}

/// Whether the row of `name` ends with `details`.
fn row_shows(frame: &str, name: &str, details: &str) -> bool {
    frame.lines().any(|line| {
        let line = line.replace(screen::SELECTED_MARK, " ");
        (line.contains(&format!("◉ {name} ")) || line.ends_with(&format!("◉ {name}")))
            && line.trim_end().ends_with(details)
    })
}

/// Whether the row of `name` is the one the selection fill marks.
fn is_selected(frame: &str, name: &str) -> bool {
    frame
        .lines()
        .any(|line| line.starts_with(&format!("◉{}{name} ", screen::SELECTED_MARK)))
}

/// The input dialog's rows: those after its top rule.
pub(crate) fn dialog(frame: &str) -> String {
    let lines = frame.lines().collect::<Vec<_>>();
    let top = lines
        .iter()
        .rposition(|line| !line.is_empty() && line.chars().all(|character| character == '▔'))
        .unwrap_or_else(|| panic!("no input dialog:\n{frame}"));
    lines[top + 1..].join("\n")
}

/// The dialog's text with its rows joined, so a wrapped message reads as one.
pub(crate) fn dialog_text(frame: &str) -> String {
    dialog(frame)
        .replace(screen::SELECTED_MARK, " ")
        .lines()
        .map(str::trim)
        .collect()
}

fn panel_is_open(frame: &str) -> bool {
    frame.contains("Plugins ·") || frame.contains("Ticks ·")
}

#[test]
fn the_ticks_panel_installs_updates_logs_out_and_uninstalls() {
    let fixture = Fixture::new();
    fixture.install_official(&fixture.package_with_login("alpha", "1.0.0"));
    install_from_path(&fixture, "tick", &fixture.package_named("beta", "1.0.0"));
    let server = Server::start();
    let (alpha_archive, alpha_sha256) = archive_of(&fixture.package_with_login("alpha", "2.0.0"));
    let (sample_archive, sample_sha256) = archive_of(&fixture.package_named("sample", "1.0.0"));
    let alpha_url = server.serve("/tick-alpha-2.0.0.tar.gz", Reply::Body(alpha_archive));
    let sample_url = server.serve("/tick-sample-1.0.0.tar.gz", Reply::Body(sample_archive));
    let marketplace = fixture.marketplace_file(
        "marketplace",
        &marketplace_json(json!([
            offered(
                "alpha",
                "2.0.0",
                &[(CURRENT_TARGET, &alpha_url, &alpha_sha256)]
            ),
            offered(
                "sample",
                "1.0.0",
                &[(CURRENT_TARGET, &sample_url, &sample_sha256)]
            ),
        ])),
    );
    let mut terminal = Terminal::start(&fixture, &marketplace);
    terminal.wait_for_frame(0, "the prompt", |frame| frame.contains("❯"));

    let frame = terminal.press(b"/ticks\r", "the Installed page", |frame| {
        row_shows(frame, "alpha", "1.0.0 · needs login")
            && row_shows(frame, "beta", "1.0.0 · no login needed")
    });
    assert!(
        frame
            .replace(screen::SELECTED_MARK, " ")
            .contains("Ticks · [Installed]  Available"),
        "{frame}"
    );
    assert!(
        is_selected(&frame, "alpha") && !is_selected(&frame, "beta"),
        "{frame}"
    );
    assert!(
        frame.contains("enter actions") && frame.contains("esc close"),
        "{frame}"
    );
    assert!(
        !dialog(&frame).contains('❯'),
        "the package list has no cursor:\n{frame}"
    );
    for (key, selected) in [
        (&b"j"[..], "beta"),
        (b"k", "alpha"),
        (CONTROL_N, "beta"),
        (CONTROL_P, "alpha"),
    ] {
        terminal.press(key, &format!("{selected} selected"), |frame| {
            is_selected(frame, selected)
        });
    }

    let frame = terminal.press(TAB, "the Marketplace page", |frame| {
        frame.contains("[Available]") && row_shows(frame, "sample", "1.0.0 · not installed")
    });
    assert!(
        row_shows(&frame, "alpha", "2.0.0 · installed 1.0.0"),
        "{frame}"
    );
    assert!(frame.contains("◉ sample · The sample package"), "{frame}");
    assert!(
        row_shows(&frame, "sample", "tickloop · 1.0.0 · not installed"),
        "{frame}"
    );
    terminal.press(b"\r", "the update", |frame| {
        frame.contains("Updated Tick alpha@tickloop from 1.0.0 to 2.0.0")
            && dialog_text(frame).contains("Its Tick program runs as you when opened")
            && !frame.contains("new Chat")
            && row_shows(frame, "alpha", "2.0.0 · installed")
    });
    terminal.press(b"j", "sample selected", |frame| {
        is_selected(frame, "sample")
    });
    terminal.press(b"\r", "the install", |frame| {
        frame.contains("Installed Tick sample@tickloop 1.0.0")
            && row_shows(frame, "sample", "1.0.0 · installed")
    });
    assert!(
        fixture
            .root
            .join("config/plugins/cache/tickloop/sample/1.0.0/bin/tick-sample")
            .exists()
    );

    terminal.press(b"\x1b[D", "the Installed page again", |frame| {
        frame.contains("[Installed]")
            && row_shows(frame, "alpha", "2.0.0 · needs login")
            && row_shows(frame, "sample", "1.0.0 · no login needed")
    });
    let frame = terminal.press(b"k\r", "alpha's actions", |frame| {
        frame.contains("alpha 2.0.0") && frame.contains("❯ Log in")
    });
    assert!(
        frame.contains("  Log out") && frame.contains("  Disable"),
        "{frame}"
    );
    assert!(!frame.contains("Update to"), "{frame}");
    assert!(frame.contains("enter confirm · esc back"), "{frame}");
    input_dialog::assert_frame(&frame, usize::from(ROWS), "Ticks");

    // A login saved elsewhere shows once the list is read again; logging out deletes it.
    terminal.press(ESCAPE, "the list again", |frame| {
        !frame.contains("❯ Log in")
    });
    save_login(&fixture, "alpha");
    terminal.press(b"r", "the login read again", |frame| {
        row_shows(frame, "alpha", "2.0.0 · logged in")
    });

    terminal.press(b"\r", "alpha's actions", |frame| frame.contains("❯ Log in"));
    terminal.press(CONTROL_N, "log out selected", |frame| {
        frame.contains("❯ Log out")
    });
    terminal.press(b"\r", "the logout", |frame| {
        frame.contains("Logged out of alpha") && row_shows(frame, "alpha", "2.0.0 · needs login")
    });
    assert!(
        !fixture
            .root
            .join("config/credentials/alpha/oauth2.json")
            .exists()
    );
    terminal.assert_alive();

    terminal.press(b"jj", "sample selected", |frame| {
        is_selected(frame, "sample")
    });
    let frame = terminal.press(b"\r", "sample's actions", |frame| {
        frame.contains("❯ Update")
    });
    assert!(!frame.contains("Log in"), "{frame}");
    terminal.press(ESCAPE, "the list", |frame| !frame.contains("❯ Update"));
    terminal.press(b"x", "uninstall confirmation", |frame| {
        frame.contains("Press x again to uninstall sample")
    });
    terminal.press(b"x", "the uninstall", |frame| {
        frame.contains("Uninstalled sample 1.0.0; its running Ticks keep running until closed")
            && !frame.contains("◉ sample")
    });
    assert!(
        !fixture
            .root
            .join("config/plugins/cache/tickloop/sample")
            .exists()
    );

    let frame = terminal.press(ESCAPE, "the closed panel", |frame| !panel_is_open(frame));
    assert!(frame.contains("────"), "the input is back:\n{frame}");
    let listed = succeeded(&fixture.tickloop(&["tick", "list"]));
    assert!(
        listed.starts_with("alpha@tickloop  2.0.0  enabled  needs login"),
        "{listed}"
    );
    terminal.exit();
}

#[test]
fn the_installed_page_works_while_the_marketplace_cannot_be_read() {
    let fixture = Fixture::new();
    fixture.install_official(&fixture.package_with_login("alpha", "1.0.0"));
    let missing = path_text(&fixture.root.join("missing.json"));
    let mut terminal = Terminal::start(&fixture, &missing);
    terminal.wait_for_frame(0, "the prompt", |frame| frame.contains("❯"));
    terminal.press(b"/ticks\r", "the Installed page", |frame| {
        row_shows(frame, "alpha", "1.0.0 · needs login")
    });
    let frame = terminal.press(b"\r", "alpha's actions", |frame| frame.contains("❯ Log in"));
    assert!(!frame.contains("Update to"), "{frame}");
    terminal.press(ESCAPE, "the list again", |frame| {
        !frame.contains("❯ Log in")
    });
    let frame = terminal.press(TAB, "the Marketplace page", |frame| {
        frame.contains("[Available]") && frame.contains("cannot read the Tick marketplace at")
    });
    assert!(
        dialog_text(&frame).contains("missing.json: No such file or directory"),
        "{frame}"
    );
    terminal.press(TAB, "the Installed page again", |frame| {
        frame.contains("[Installed]") && row_shows(frame, "alpha", "1.0.0 · needs login")
    });
    terminal.press(ESCAPE, "the closed panel", |frame| !panel_is_open(frame));
    terminal.exit();
}

#[test]
fn the_ticks_panel_logs_in_with_a_device_code_or_a_key() {
    let fixture = Fixture::new();
    let (server, other) = (AuthorizationServer::start(), AuthorizationServer::start());
    for package in [
        fixture.package_with_auth("alpha", "1.0.0", oauth2(&[("Local", server.issuer())])),
        fixture.package_with_auth(
            "multi",
            "1.0.0",
            oauth2(&[
                ("China mainland", server.issuer()),
                ("Global", other.issuer()),
            ]),
        ),
        fixture.package_with_auth(
            "keyed",
            "1.0.0",
            json!({"type": "api_key", "env": "KEYED_API_KEY", "label": "Keyed API key"}),
        ),
    ] {
        install_from_path(&fixture, "tick", &package);
    }
    let token_path = fixture.root.join("config/credentials/alpha/oauth2.json");
    let missing = path_text(&fixture.root.join("missing.json"));
    let mut terminal = Terminal::start(&fixture, &missing);
    terminal.wait_for_frame(0, "the prompt", |frame| frame.contains("❯"));
    terminal.press(b"/ticks\r", "the Installed page", |frame| {
        ["alpha", "keyed", "multi"]
            .iter()
            .all(|name| row_shows(frame, name, "1.0.0 · needs login"))
    });

    // The panel shows the link and code, and reports what the server answers.
    server.answer_polls([TokenReply::Denied]);
    terminal.press(b"\r", "alpha's actions", |frame| frame.contains("❯ Log in"));
    let frame = terminal.press(b"\r", "the link and code", |frame| {
        frame.contains(&format!("Enter code: {USER_CODE}"))
    });
    let shown = dialog_text(&frame);
    assert!(
        shown.contains("Log in to alpha · Local")
            && shown.contains("Open this link on your own device:")
            && shown.contains(&format!("{}/verify?user_code={USER_CODE}", server.issuer()))
            && shown.contains("Waiting for the authorization of alpha. esc cancels.")
            && shown.contains("❯ Cancel login"),
        "{frame}"
    );
    input_dialog::assert_frame(&frame, usize::from(ROWS), "Ticks");
    terminal.wait_for_frame(terminal.mark(), "the refusal", |frame| {
        dialog_text(frame).contains("Cannot log in to alpha: the authorization was denied")
            && row_shows(frame, "alpha", "1.0.0 · needs login")
    });

    server.edit_device_authorization(|device| device["expires_in"] = json!(2));
    terminal.press(b"\r", "alpha's actions", |frame| frame.contains("❯ Log in"));
    terminal.press(b"\r", "the expiry", |frame| {
        dialog_text(frame).contains(
            "Cannot log in to alpha: the code expired before the authorization was approved",
        )
    });
    server.edit_device_authorization(|device| device["expires_in"] = json!(600));

    // esc cancels a login; a login under way leaves nothing behind.
    terminal.press(b"\r", "alpha's actions", |frame| frame.contains("❯ Log in"));
    terminal.press(b"\r", "the code", |frame| {
        frame.contains(&format!("Enter code: {USER_CODE}"))
    });
    terminal.press(ESCAPE, "the cancelled login", |frame| {
        frame.contains("Cancelled the login to alpha")
            && row_shows(frame, "alpha", "1.0.0 · needs login")
    });
    assert!(!token_path.exists());

    server.answer_polls([TokenReply::Pending, TokenReply::Approve]);
    terminal.press(b"\r", "alpha's actions", |frame| frame.contains("❯ Log in"));
    terminal.press(b"\r", "the login", |frame| {
        frame.contains("Logged in to alpha") && row_shows(frame, "alpha", "1.0.0 · logged in")
    });
    let saved: serde_json::Value = serde_json::from_slice(&fs::read(&token_path).unwrap()).unwrap();
    assert_eq!(saved["issuer"], server.issuer());
    terminal.assert_alive();

    // A package with several issuers offers a login at each.
    terminal.press(b"jj", "multi selected", |frame| is_selected(frame, "multi"));
    let frame = terminal.press(b"\r", "multi's actions", |frame| {
        frame.contains("❯ Log in · China mainland")
    });
    assert!(frame.contains("  Log in · Global"), "{frame}");
    terminal.press(ESCAPE, "the list again", |frame| {
        !frame.contains("Log in · Global")
    });

    // A key is typed masked; j and k are text there.
    terminal.press(b"k", "keyed selected", |frame| is_selected(frame, "keyed"));
    terminal.press(b"\r", "keyed's actions", |frame| frame.contains("❯ Log in"));
    let frame = terminal.press(b"\r", "the key entry", |frame| {
        dialog_text(frame).contains("Enter the Keyed API key.")
    });
    assert!(frame.contains("enter save · esc cancel"), "{frame}");
    let frame = terminal.press(b"jk-secret", "the masked key", |frame| {
        frame.contains("Keyed API key: *********")
    });
    assert!(!frame.contains("jk-secret"), "{frame}");
    terminal.press(b"\r", "the saved key", |frame| {
        frame.contains("Saved the key for keyed") && row_shows(frame, "keyed", "1.0.0 · logged in")
    });
    assert_eq!(
        fs::read(fixture.root.join("config/credentials/keyed/api_key")).unwrap(),
        b"jk-secret"
    );
    terminal.press(b"\r", "keyed's actions", |frame| frame.contains("❯ Log in"));
    terminal.press(CONTROL_N, "log out selected", |frame| {
        frame.contains("❯ Log out")
    });
    terminal.press(b"\r", "the logout", |frame| {
        frame.contains("Logged out of keyed") && row_shows(frame, "keyed", "1.0.0 · needs login")
    });
    terminal.press(ESCAPE, "the closed panel", |frame| !panel_is_open(frame));
    terminal.exit();
}

#[test]
fn ticks_and_plugins_are_separate_commands_and_panels() {
    let fixture = Fixture::new();
    install_from_path(&fixture, "tick", &fixture.package_named("alpha", "1.0.0"));
    let notes = fixture.root.join("notes");
    fs::create_dir_all(notes.join(".claude-plugin")).unwrap();
    fs::write(
        notes.join(".claude-plugin/plugin.json"),
        r#"{"name":"notes","version":"1.0.0"}"#,
    )
    .unwrap();
    install_from_path(&fixture, "plugin", &notes);
    let marketplace = fixture.marketplace_file("empty", &marketplace_json(json!([])));
    let mut terminal = Terminal::start(&fixture, &marketplace);
    terminal.wait_for_frame(0, "the prompt", |frame| frame.contains("❯"));
    terminal.press(b"/tic", "the /ticks completion", |frame| {
        frame.contains("/ticks") && frame.contains("log in to Ticks")
    });
    let frame = terminal.press(b"ks\r", "the Ticks panel", |frame| {
        frame
            .replace(screen::SELECTED_MARK, " ")
            .contains("Ticks · [Installed]  Available")
            && row_shows(frame, "alpha", "1.0.0 · no login needed")
    });
    assert!(
        !frame.contains("◉ notes") && !frame.contains("renamed"),
        "{frame}"
    );
    terminal.press(ESCAPE, "closed Ticks", |frame| !panel_is_open(frame));
    let frame = terminal.press(b"/plugins\r", "the Plugins panel", |frame| {
        frame
            .replace(screen::SELECTED_MARK, " ")
            .contains("Plugins · [Installed]  Available")
            && row_shows(frame, "notes", "local · enabled · 1.0.0 · no login needed")
    });
    assert!(!frame.contains("◉ alpha"), "{frame}");
    terminal.press(ESCAPE, "closed Plugins", |frame| !panel_is_open(frame));
    terminal.exit();
}

#[test]
fn installed_plugins_show_components_and_can_be_disabled_and_enabled() {
    let fixture = Fixture::new();
    let directory = fixture.root.join("notes");
    fs::create_dir_all(directory.join(".claude-plugin")).unwrap();
    fs::write(
        directory.join(".claude-plugin/plugin.json"),
        r#"{"name":"notes","version":"1.0.0","mcpServers":{"notes":{}},"hooks":{"hooks":{}}}"#,
    )
    .unwrap();
    install_from_path(&fixture, "plugin", &directory);
    let marketplace = fixture.marketplace_file("empty", &marketplace_json(json!([])));
    let mut terminal = Terminal::start(&fixture, &marketplace);
    terminal.wait_for_frame(0, "the prompt", |frame| frame.contains("❯"));
    let frame = terminal.press(b"/plugins\r", "installed plugin metadata", |frame| {
        row_shows(
            frame,
            "notes",
            "local · enabled · 1.0.0 · no login needed · 1 MCP server",
        ) && frame.contains("Not supported yet: hooks")
    });
    assert!(!frame.contains("Not supported yet: MCP"), "{frame}");
    terminal.press(b"\r", "disable action", |frame| frame.contains("❯ Disable"));
    terminal.press(b"\r", "disabled", |frame| {
        row_shows(
            frame,
            "notes",
            "local · disabled · 1.0.0 · no login needed · 1 MCP server",
        )
    });
    terminal.press(b"\r", "enable action", |frame| frame.contains("❯ Enable"));
    terminal.press(b"\r", "enabled", |frame| {
        row_shows(
            frame,
            "notes",
            "local · enabled · 1.0.0 · no login needed · 1 MCP server",
        )
    });
    terminal.press(ESCAPE, "closed", |frame| !panel_is_open(frame));
    terminal.exit();
    assert!(
        succeeded(&fixture.tickloop(&["plugin", "list"])).contains("notes@local  1.0.0  enabled")
    );
}

#[test]
fn available_plugins_filter_hundreds_of_entries_and_keep_text_keys_in_the_filter() {
    let fixture = Fixture::new();
    let server = Server::start();
    let (archive, sha256) = archive_of(&fixture.package_named("jk-349", "1.0.0"));
    let url = server.serve("/jk-349.tar.gz", Reply::Body(archive));
    let packages = (0..350)
        .map(|index| {
            let downloads = if index == 349 {
                vec![(CURRENT_TARGET, url.as_str(), sha256.as_str())]
            } else {
                vec![]
            };
            offered(&format!("jk-{index:03}"), "1.0.0", &downloads)
        })
        .collect::<Vec<_>>();
    let editor = fixture.root.join("filter-editor");
    fs::write(&editor, "#!/bin/sh\nprintf 'jk-349\\n' > \"$1\"\n").unwrap();
    fs::set_permissions(&editor, fs::Permissions::from_mode(0o700)).unwrap();
    let marketplace = fixture.marketplace_file("many", &marketplace_json(json!(packages)));
    let mut terminal = Terminal::start(&fixture, &marketplace);
    terminal.wait_for_frame(0, "the prompt", |frame| frame.contains("❯"));
    terminal.press(b"/ticks\r", "ticks", panel_is_open);
    terminal.press(TAB, "available plugins", |frame| {
        frame.contains("[Available]") && frame.contains("jk-000")
    });
    terminal.press(b"/jk-34", "filtered plugins", |frame| {
        frame.contains("Filter: jk-34") && is_selected(frame, "jk-340") && !frame.contains("jk-000")
    });
    terminal.press(CONTROL_N, "next filtered row", |frame| {
        is_selected(frame, "jk-341")
    });
    terminal.press(CONTROL_P, "previous filtered row", |frame| {
        is_selected(frame, "jk-340")
    });
    terminal.press(b"xx", "x stays text in the filter", |frame| {
        frame.contains("Filter: jk-34xx") && frame.contains("No Ticks match")
    });
    terminal.press(b"\x7f\x7f", "backspace restores matches", |frame| {
        is_selected(frame, "jk-340")
    });
    terminal.press(ESCAPE, "leave filter", |frame| {
        frame.contains("/ filter") && panel_is_open(frame)
    });
    terminal.press(b"j", "list navigation restored", |frame| {
        is_selected(frame, "jk-341")
    });
    terminal.press(b"\r", "unavailable source explanation", |frame| {
        frame.contains("no prebuilt program")
    });
    terminal.press(b"/\x07", "external editor changes filter", |frame| {
        frame.contains("Filter: jk-349") && is_selected(frame, "jk-349")
    });
    terminal.press(b"\r", "install the filtered selection", |frame| {
        frame.contains("Installed Tick jk-349@tickloop 1.0.0")
            && row_shows(frame, "jk-349", "1.0.0 · installed")
    });
    terminal.press(ESCAPE, "leave edited filter", |frame| {
        frame.contains("/ filter")
    });
    terminal.press(TAB, "installed filtered plugin", |frame| {
        frame.contains("[Installed]") && row_shows(frame, "jk-349", "1.0.0 · no login needed")
    });
    terminal.press(ESCAPE, "closed", |frame| !panel_is_open(frame));
    terminal.exit();
}

#[test]
fn uninstall_requires_two_plain_x_presses_on_the_same_plugin() {
    let fixture = Fixture::new();
    install_from_path(&fixture, "tick", &fixture.package_named("alpha", "1.0.0"));
    install_from_path(&fixture, "tick", &fixture.package_named("beta", "1.0.0"));
    save_login(&fixture, "alpha");
    let marketplace = fixture.marketplace_file("empty", &marketplace_json(json!([])));
    let mut terminal = Terminal::start(&fixture, &marketplace);
    terminal.wait_for_frame(0, "the prompt", |frame| frame.contains("❯"));
    terminal.press(b"/ticks\r", "installed Ticks", |frame| {
        row_shows(frame, "alpha", "1.0.0 · no login needed")
    });
    terminal.press(b"x", "uninstall armed", |frame| {
        frame.contains("Press x again to uninstall alpha")
    });
    terminal.press(
        b"\x18\x18",
        "control x cancels without uninstalling",
        |frame| !frame.contains("Press x again") && is_selected(frame, "alpha"),
    );
    terminal.press(b"x", "plain x arms again", |frame| {
        frame.contains("Press x again to uninstall alpha")
    });
    terminal.press(b"j", "selection cancels uninstall", |frame| {
        is_selected(frame, "beta") && !frame.contains("Press x again")
    });
    terminal.press(b"x", "beta armed", |frame| {
        frame.contains("Press x again to uninstall beta")
    });
    terminal.press(b"k", "alpha selected", |frame| is_selected(frame, "alpha"));
    terminal.press(b"x", "confirmation will expire", |frame| {
        frame.contains("Press x again to uninstall alpha")
    });
    terminal.wait_for_frame(terminal.mark(), "confirmation expired", |frame| {
        !frame.contains("Press x again") && frame.contains("x uninstall")
    });
    terminal.press(b"x", "alpha armed again", |frame| {
        frame.contains("Press x again to uninstall alpha")
    });
    terminal.press(b"x", "alpha uninstalled", |frame| {
        frame.contains("Uninstalled alpha") && !frame.contains("◉ alpha")
    });
    assert!(
        fixture
            .root
            .join("config/credentials/alpha/oauth2.json")
            .exists()
    );
    assert!(row_shows(
        &terminal.frame(),
        "beta",
        "1.0.0 · no login needed"
    ));
    terminal.press(ESCAPE, "closed", |frame| !panel_is_open(frame));
    terminal.exit();
}

#[test]
fn a_short_terminal_cannot_confirm_a_hidden_plugin_uninstall() {
    let fixture = Fixture::new();
    install_from_path(&fixture, "tick", &fixture.package_named("alpha", "1.0.0"));
    let marketplace = fixture.marketplace_file("empty", &marketplace_json(json!([])));
    let mut terminal = Terminal::start(&fixture, &marketplace);
    terminal.wait_for_frame(0, "the prompt", |frame| frame.contains("❯"));
    terminal.press(b"/ticks\r", "installed", |frame| {
        row_shows(frame, "alpha", "1.0.0 · no login needed")
    });
    terminal.press(b"x", "uninstall armed", |frame| {
        frame.contains("Press x again to uninstall alpha")
    });
    let mark = terminal.mark();
    terminal.resize(30, 6);
    terminal.wait_for_frame(mark, "resize instead of hidden confirmation", |frame| {
        frame.contains("Resize · esc back")
    });
    terminal.send(b"x");
    terminal.press(ESCAPE, "closed small panel", |frame| {
        !panel_is_open(frame) && !frame.contains("Resize")
    });
    let mark = terminal.mark();
    terminal.resize(COLUMNS, ROWS);
    terminal.wait_for_frame(mark, "full terminal", |frame| frame.contains("❯"));
    terminal.press(b"/ticks\r", "Tick preserved", |frame| {
        row_shows(frame, "alpha", "1.0.0 · no login needed")
    });
    terminal.press(ESCAPE, "closed", |frame| !panel_is_open(frame));
    terminal.exit();
}

#[test]
fn available_plugins_install_from_each_marketplace_despite_another_marketplace_error() {
    use crate::marketplace_commands::{marketplace_directory, write_json};
    let fixture = Fixture::new();
    let team = marketplace_directory(
        &fixture,
        "team",
        "team",
        json!([
            {"name":"notes", "source":"./notes", "version":"1.0.0", "description":"Team notes"},
            {"name":"unsupported", "source":{"source":"npm", "package":"unsupported"}}
        ]),
    );
    write_json(
        &team.join("notes/.claude-plugin/plugin.json"),
        &json!({"name":"Notebook", "version":"1.0.0"}),
    );
    succeeded(&fixture.tickloop(&["plugin", "marketplace", "add", &path_text(&team)]));
    let marketplace = fixture.marketplace_file("empty", &marketplace_json(json!([])));
    let mut terminal = Terminal::start(&fixture, &marketplace);
    terminal.wait_for_frame(0, "prompt", |frame| frame.contains("❯"));
    let frame = terminal.press(b"/plugins\r", "installed page", panel_is_open);
    assert!(!dialog(&frame).contains("cannot read marketplace"));
    terminal.press(TAB, "all marketplaces", |frame| {
        frame.contains("notes")
            && frame.contains("team · 1.0.0 · not installed")
            && frame.contains("cannot read marketplace claude-plugins-official")
    });
    terminal.press(b"jj", "marketplace error selection", |frame| {
        frame.lines().any(|line| {
            line.contains(screen::SELECTED_MARK)
                && line
                    .replace(screen::SELECTED_MARK, " ")
                    .contains("cannot read marketplace claude-plugins-official")
        }) && !frame.contains("Resize")
            && !frame.contains("enter install/update")
    });
    terminal.press(b"/notes\r", "install explicit marketplace", |frame| {
        frame.contains("Installed plugin notes@team 1.0.0")
            && dialog_text(frame).contains("Its manifest calls it Notebook")
    });
    terminal.press(ESCAPE, "leave filter", |frame| frame.contains("/ filter"));
    terminal.press(TAB, "installed alias metadata", |frame| {
        frame.contains("manifest name: Notebook")
            && row_shows(frame, "notes", "team · enabled · 1.0.0 · no login needed")
            && !dialog(frame).contains("cannot read marketplace")
    });
    terminal.press(ESCAPE, "closed", |frame| !panel_is_open(frame));
    terminal.exit();
    assert!(succeeded(&fixture.tickloop(&["plugin", "list"])).contains("notes@team"));
}

#[test]
fn plugin_updates_refresh_git_revisions_even_when_the_display_version_does_not_change() {
    use crate::marketplace_commands::{
        commit_all, file_url, marketplace_directory, write_json, write_skill,
    };
    let fixture = Fixture::new();
    let team = marketplace_directory(
        &fixture,
        "git-team",
        "team",
        json!([
            {"name":"notes", "source":"./notes", "version":"1.0.0"}
        ]),
    );
    write_json(
        &team.join("notes/.claude-plugin/plugin.json"),
        &json!({"name":"notes", "version":"1.0.0"}),
    );
    let skill = team.join("notes/skills/check");
    write_skill(&skill, "check", "First revision");
    commit_all(&team, "first");
    succeeded(&fixture.tickloop(&["plugin", "marketplace", "add", &file_url(&team)]));
    succeeded(&fixture.tickloop(&["plugin", "install", "notes@team"]));
    succeeded(&fixture.tickloop(&["plugin", "disable", "notes"]));
    let marketplace = fixture.marketplace_file("empty", &marketplace_json(json!([])));
    let mut terminal = Terminal::start(&fixture, &marketplace);
    terminal.wait_for_frame(0, "prompt", |frame| frame.contains("❯"));
    terminal.press(b"/plugins\r", "disabled installed plugin", |frame| {
        row_shows(frame, "notes", "team · disabled · 1.0.0 · no login needed")
    });
    write_skill(&skill, "check", "Second revision");
    let second = commit_all(&team, "second");
    terminal.press(b"\r", "update action for same version", |frame| {
        frame.contains("❯ Update")
    });
    terminal.press(b"\r", "updated revision", |frame| {
        dialog_text(frame).contains("Reinstalled plugin notes@team 1.0.0")
            && frame.contains("It stays disabled")
    });
    assert!(
        fs::read_to_string(fixture.root.join(format!(
            "config/plugins/cache/team/notes/{}/skills/check/SKILL.md",
            &second[..12]
        )))
        .unwrap()
        .contains("Second revision")
    );
    terminal.press(TAB, "available installed version", |frame| {
        frame.contains("[Available]") && row_shows(frame, "notes", "team · 1.0.0 · installed")
    });
    write_skill(&skill, "check", "Third revision");
    write_json(
        &team.join("notes/.claude-plugin/plugin.json"),
        &json!({"name":"notes", "version":"2.0.0"}),
    );
    write_json(
        &team.join(".claude-plugin/marketplace.json"),
        &json!({"name":"team", "plugins":[{"name":"notes","source":"./notes","version":"2.0.0"}]}),
    );
    let third = commit_all(&team, "third");
    terminal.press(b"/notes\r", "available update refreshes again", |frame| {
        dialog_text(frame).contains("Updated plugin notes@team from 1.0.0 to 2.0.0")
            && row_shows(frame, "notes", "team · 2.0.0 · installed")
            && frame.contains("It stays disabled")
    });
    assert!(
        fs::read_to_string(fixture.root.join(format!(
            "config/plugins/cache/team/notes/{}/skills/check/SKILL.md",
            &third[..12]
        )))
        .unwrap()
        .contains("Third revision")
    );
    terminal.press(b"\r", "unchanged revision is up to date", |frame| {
        frame.contains("Plugin notes@team 2.0.0 is up to date")
    });
    terminal.press(ESCAPE, "leave filter", |frame| frame.contains("/ filter"));
    terminal.press(ESCAPE, "closed", |frame| !panel_is_open(frame));
    terminal.exit();
}

#[test]
fn available_status_distinguishes_marketplace_ownership_and_broken_plugins() {
    use crate::marketplace_commands::{marketplace_directory, write_json};
    let fixture = Fixture::new();
    for market in ["first", "second"] {
        let source = marketplace_directory(
            &fixture,
            market,
            market,
            json!([
                {"name":"notes", "source":"./notes", "version":"1.0.0"},
                {"name":"unsupported", "source":{"source":"npm", "package":"unsupported"}}
            ]),
        );
        write_json(
            &source.join("notes/.claude-plugin/plugin.json"),
            &json!({"name":"notes", "version":"1.0.0"}),
        );
        succeeded(&fixture.tickloop(&["plugin", "marketplace", "add", &path_text(&source)]));
    }
    succeeded(&fixture.tickloop(&["plugin", "install", "notes@first"]));
    succeeded(&fixture.tickloop(&["plugin", "disable", "notes"]));
    let cached = fixture.root.join("config/plugins/cache/first/notes/local");
    assert!(cached.exists());
    fs::remove_dir_all(cached).unwrap();
    let marketplace = fixture.marketplace_file("empty", &marketplace_json(json!([])));
    let mut terminal = Terminal::start(&fixture, &marketplace);
    terminal.wait_for_frame(0, "prompt", |frame| frame.contains("❯"));
    terminal.press(b"/plugins\r", "unusable plugin metadata", |frame| {
        row_shows(
            frame,
            "notes",
            "first · disabled · 1.0.0 · unusable; reinstall",
        )
    });
    terminal.press(TAB, "available ownership states", |frame| {
        frame.contains("installed 1.0.0 but unusable")
            && frame.contains("not installed; notes@first is installed")
    });
    terminal.press(
        b"/second\r",
        "cross-marketplace replacement refused",
        |frame| {
            dialog_text(frame).contains(
                "plugin notes is already installed from marketplace first; uninstall it first",
            )
        },
    );
    terminal.press(b"\x15unsupported", "unsupported source", |frame| {
        frame.contains("npm sources are not supported yet")
    });
    terminal.press(b"\r", "unsupported install refused", |frame| {
        dialog_text(frame)
            .contains("cannot install unsupported@first: npm sources are not supported yet")
    });
    terminal.press(ESCAPE, "leave filter", |frame| frame.contains("/ filter"));
    terminal.press(TAB, "installed owner", |frame| {
        frame.contains("[Installed]")
    });
    terminal.press(b"\r", "repair action", |frame| frame.contains("❯ Update"));
    terminal.press(b"\r", "repaired original marketplace", |frame| {
        row_shows(frame, "notes", "first · disabled · 1.0.0 · no login needed")
    });
    terminal.press(ESCAPE, "closed", |frame| !panel_is_open(frame));
    terminal.exit();
    assert!(
        succeeded(&fixture.tickloop(&["plugin", "list"])).contains("notes@first  1.0.0  disabled")
    );
}

#[test]
fn plugin_user_config_page_masks_secrets_and_reconfigures_saved_values() {
    let fixture = Fixture::new();
    let directory = fixture.root.join("configured");
    fs::create_dir_all(directory.join(".claude-plugin")).unwrap();
    fs::write(directory.join(".claude-plugin/plugin.json"), json!({
        "name":"configured", "userConfig": {
            "key":{"type":"string","title":"Private key","sensitive":true,"required":true},
            "workspace":{"type":"directory","title":"Workspace","required":true}
        },"mcpServers":{"service":{"url":"https://example.com/mcp","headers":{"Key":"${user_config.key}"}}}
    }).to_string()).unwrap();
    install_from_path(&fixture, "plugin", &directory);
    let marketplace = fixture.marketplace_file("empty", &marketplace_json(json!([])));
    let mut terminal = Terminal::start(&fixture, &marketplace);
    terminal.wait_for_frame(0, "prompt", |frame| frame.contains("❯"));
    terminal.press(b"/plugins\r", "configuration status", |frame| {
        frame.contains("needs configuration")
    });
    terminal.press(b"\r", "configure action", |frame| {
        frame.contains("Configure")
    });
    terminal.press(b"\r", "private field", |frame| {
        frame.contains("Private key") && frame.contains("Configure configured")
    });
    let shown = terminal.press(b"panel-private-key", "masked input", |frame| {
        frame.contains("*****************")
    });
    assert!(!shown.contains("panel-private-key"));
    terminal.press(b"\x1b[D", "masked cursor moves left", |frame| {
        frame.contains("****************▏*")
    });
    terminal.press(b"X", "insert at masked cursor", |frame| {
        frame.contains("*****************▏*")
    });
    terminal.press(b"\r", "workspace field", |frame| {
        frame.contains("Workspace")
    });
    terminal.press(b"/tmp/jk\r", "saved", |frame| {
        frame.contains("configured") && !frame.contains("Configure configured")
    });
    let private = fixture
        .root
        .join("config/credentials/configured/user-config.json");
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&fs::read(private).unwrap()).unwrap()["key"],
        "panel-private-keXy"
    );
    let public = fs::read_to_string(fixture.root.join("config/settings.json")).unwrap();
    assert!(public.contains("/tmp/jk"));
    assert!(!public.contains("panel-private-key"));
    terminal.press(b"\r", "configure action", |frame| {
        frame.contains("Configure")
    });
    terminal.press(b"\r", "saved private field", |frame| {
        frame.contains("Private key") && frame.contains("********")
    });
    terminal.press(ESCAPE, "cancel configuration", |frame| {
        !frame.contains("Configure configured")
    });
    terminal.press(ESCAPE, "close panel", |frame| !panel_is_open(frame));
    terminal.exit();
    assert!(
        !terminal
            .output
            .lock()
            .unwrap()
            .contains("panel-private-key")
    );
}

#[test]
fn installing_a_plugin_in_the_panel_opens_its_missing_configuration() {
    use crate::marketplace_commands::{marketplace_directory, write_json};
    let fixture = Fixture::new();
    let team = marketplace_directory(
        &fixture,
        "team",
        "team",
        json!([{"name":"configured","source":"./configured"}]),
    );
    write_json(
        &team.join("configured/.claude-plugin/plugin.json"),
        &json!({
            "name":"configured", "userConfig":{"key":{"type":"string","title":"Private key","sensitive":true,"required":true}}
        }),
    );
    succeeded(&fixture.tickloop(&["plugin", "marketplace", "add", &path_text(&team)]));
    let marketplace = fixture.marketplace_file("empty", &marketplace_json(json!([])));
    let mut terminal = Terminal::start(&fixture, &marketplace);
    terminal.wait_for_frame(0, "prompt", |frame| frame.contains("❯"));
    terminal.press(b"/plugins\r", "installed page", panel_is_open);
    terminal.press(TAB, "available page", |frame| {
        frame.contains("[Available]") && frame.contains("configured")
    });
    terminal.press(
        b"/configured\r",
        "configuration after installation",
        |frame| frame.contains("Configure configured") && frame.contains("Private key"),
    );
    terminal.press(b"cancelled-secret", "masked", |frame| {
        frame.contains("****************")
    });
    terminal.press(ESCAPE, "cancel", |frame| {
        !frame.contains("Configure configured")
    });
    assert!(
        !fixture
            .root
            .join("config/credentials/configured/user-config.json")
            .exists()
    );
    terminal.press(ESCAPE, "leave filter", |frame| {
        !frame.contains("esc leave filter")
    });
    terminal.press(ESCAPE, "close panel", |frame| !panel_is_open(frame));
    terminal.exit();
    assert!(!terminal.output.lock().unwrap().contains("cancelled-secret"));
}
