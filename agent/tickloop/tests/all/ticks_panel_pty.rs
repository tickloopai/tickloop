//! `/ticks` in the Agent's terminal, against a private config root, with a local marketplace file
//! and a local HTTP server for its downloads.

use std::{
    fs,
    io::Write,
    path::Path,
    sync::{Arc, Mutex},
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use portable_pty::{CommandBuilder, PtySize, native_pty_system};
use serde_json::json;
use tick_packages::{CURRENT_TARGET, OAuthToken, PackageCredentials};
use tick_test_support::authorization_server::{AuthorizationServer, TokenReply, USER_CODE};
use tickloop_agent_test_support::{input_dialog, screen};

use crate::tick_package_commands::{
    Fixture, Reply, Server, archive_of, marketplace_json, oauth2, offered, path_text, succeeded,
};

const COLUMNS: u16 = 110;
const ROWS: u16 = 36;
const TIMEOUT: Duration = Duration::from_secs(20);
const FRAME_SETTLE: Duration = Duration::from_millis(40);
const CONTROL_N: &[u8] = b"\x0e";
const CONTROL_P: &[u8] = b"\x10";
const ESCAPE: &[u8] = b"\x1b";
const TAB: &[u8] = b"\t";
struct Terminal {
    child: Box<dyn portable_pty::Child + Send + Sync>,
    writer: Box<dyn Write + Send>,
    output: Arc<Mutex<String>>,
    _master: Box<dyn portable_pty::MasterPty + Send>,
    fixture_home: std::path::PathBuf,
    config_root: std::path::PathBuf,
    runtime: std::path::PathBuf,
}

impl Terminal {
    fn start(fixture: &Fixture, marketplace: &str) -> Self {
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
        command.env("TICKLOOP_DAEMON_RUNTIME_DIR", &fixture.runtime);
        command.env("TICKLOOP_TICK_MARKETPLACE", marketplace);
        command.env("TICKLOOP_AGENT_TUI", "inline");
        command.env("TERM", "xterm-256color");
        command.env("NO_PROXY", "127.0.0.1,localhost");
        command.env("no_proxy", "127.0.0.1,localhost");
        for name in ["HERDR_ENV", "HERDR_PANE_ID", "HERDR_SOCKET_PATH", "TMUX"] {
            command.env_remove(name);
        }
        for model in tickloop_agent_model::cards::built_in_model_profiles() {
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
            runtime: fixture.runtime.clone(),
        }
    }

    fn send(&mut self, input: &[u8]) {
        self.writer.write_all(input).unwrap();
        self.writer.flush().unwrap();
    }

    fn mark(&self) -> usize {
        self.output.lock().unwrap().len()
    }

    fn frame(&self) -> String {
        screen::visible_screen_with_selection(
            &self.output.lock().unwrap(),
            usize::from(COLUMNS),
            usize::from(ROWS),
        )
    }

    /// Waits for a frame drawn after `mark` that `matches`.
    fn wait_for_frame(&self, mark: usize, label: &str, matches: impl Fn(&str) -> bool) -> String {
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
    fn press(&mut self, input: &[u8], label: &str, matches: impl Fn(&str) -> bool) -> String {
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

    fn exit(&mut self) {
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
            .env("TICKLOOP_DAEMON_RUNTIME_DIR", &self.runtime)
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

fn install_path(fixture: &Fixture, directory: &Path) {
    succeeded(&fixture.tickloop(&["tick", "install", "--path", &path_text(directory)]));
}

/// Whether the row of `name` ends with `details`.
fn row_shows(frame: &str, name: &str, details: &str) -> bool {
    frame.lines().any(|line| {
        (line.contains(&format!("◉ {name} ")) || line.ends_with(&format!("◉ {name}")))
            && line.trim_end().ends_with(details)
    })
}

/// Whether the row of `name` is the one the selection fill marks.
fn is_selected(frame: &str, name: &str) -> bool {
    frame
        .lines()
        .any(|line| line.contains(&format!("◉ {name}")) && line.starts_with(screen::SELECTED_MARK))
}

/// The input dialog's rows: those after its top rule.
fn dialog(frame: &str) -> String {
    let lines = frame.lines().collect::<Vec<_>>();
    let top = lines
        .iter()
        .rposition(|line| !line.is_empty() && line.chars().all(|character| character == '▔'))
        .unwrap_or_else(|| panic!("no input dialog:\n{frame}"));
    lines[top + 1..].join("\n")
}

/// The dialog's text with its rows joined, so a wrapped message reads as one.
fn dialog_text(frame: &str) -> String {
    dialog(frame).lines().map(str::trim).collect()
}

fn panel_is_open(frame: &str) -> bool {
    frame.contains("Tick packages · ")
}

#[test]
fn the_ticks_panel_installs_updates_logs_out_and_uninstalls() {
    let fixture = Fixture::new();
    install_path(&fixture, &fixture.package_with_login("alpha", "1.0.0"));
    install_path(&fixture, &fixture.package_named("beta", "1.0.0"));
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
        frame.contains("Tick packages · [Installed]  Marketplace"),
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
        frame.contains("[Marketplace]") && row_shows(frame, "sample", "1.0.0 · not installed")
    });
    assert!(
        row_shows(&frame, "alpha", "2.0.0 · installed 1.0.0"),
        "{frame}"
    );
    assert!(frame.contains("◉ sample · The sample package"), "{frame}");
    terminal.press(b"\r", "the update", |frame| {
        frame.contains("Updated alpha 1.0.0 to 2.0.0; its Skill loads in a new Chat (/clear)")
            && row_shows(frame, "alpha", "2.0.0 · installed")
    });
    terminal.press(b"j", "sample selected", |frame| {
        is_selected(frame, "sample")
    });
    terminal.press(b"\r", "the install", |frame| {
        frame.contains("Installed sample 1.0.0; its Skill loads in a new Chat (/clear)")
            && row_shows(frame, "sample", "1.0.0 · installed")
    });
    assert!(
        fixture
            .root
            .join("config/ticks/sample/bin/tick-sample")
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
        frame.contains("  Log out") && frame.contains("  Uninstall"),
        "{frame}"
    );
    assert!(!frame.contains("Update to"), "{frame}");
    assert!(frame.contains("enter confirm · esc back"), "{frame}");
    input_dialog::assert_frame(&frame, usize::from(ROWS), "Tick packages");

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
        frame.contains("sample 1.0.0") && frame.contains("❯ Uninstall")
    });
    assert!(
        !frame.contains("Log in"),
        "a package without login offers none:\n{frame}"
    );
    let frame = terminal.press(b"\r", "the uninstall confirmation", |frame| {
        frame.contains("Uninstall sample 1.0.0?")
    });
    assert!(frame.contains("Its saved login stays."), "{frame}");
    assert!(
        frame.contains("Its running Ticks keep running until they are closed."),
        "{frame}"
    );
    assert!(
        frame.contains("❯ Uninstall") && frame.contains("  Cancel"),
        "{frame}"
    );
    terminal.press(ESCAPE, "sample's actions again", |frame| {
        frame.contains("sample 1.0.0") && !frame.contains("Cancel")
    });
    terminal.press(b"\r", "the confirmation again", |frame| {
        frame.contains("  Cancel")
    });
    terminal.press(b"\r", "the uninstall", |frame| {
        frame.contains("Uninstalled sample 1.0.0; its running Ticks keep running until closed")
            && !frame.contains("◉ sample")
    });
    assert!(!fixture.root.join("config/ticks/sample").exists());

    let frame = terminal.press(ESCAPE, "the closed panel", |frame| !panel_is_open(frame));
    assert!(frame.contains("────"), "the input is back:\n{frame}");
    let listed = succeeded(&fixture.tickloop(&["tick", "list"]));
    assert!(listed.starts_with("alpha  2.0.0  needs login"), "{listed}");
    terminal.exit();
}

#[test]
fn the_installed_page_works_while_the_marketplace_cannot_be_read() {
    let fixture = Fixture::new();
    install_path(&fixture, &fixture.package_with_login("alpha", "1.0.0"));
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
        frame.contains("[Marketplace]") && frame.contains("cannot read the Tick marketplace at")
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
        install_path(&fixture, &package);
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
    input_dialog::assert_frame(&frame, usize::from(ROWS), "Tick packages");
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
