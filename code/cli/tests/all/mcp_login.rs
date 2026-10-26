use crate::tick_package_commands::{Fixture, succeeded};
use serde_json::{Value, json};
use std::{
    fs,
    io::{BufRead, BufReader, Write},
    process::{Child, Stdio},
    sync::mpsc,
    time::Duration,
};
use tick_oauth_test_server::{AuthorizationServer, REVOCATION_PATH};

struct Login {
    child: Child,
    lines: mpsc::Receiver<String>,
}
impl Drop for Login {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
impl Login {
    fn start(fixture: &Fixture, name: &str) -> Self {
        let mut child = fixture
            .command(&["mcp", "login", name])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let stdout = child.stdout.take().unwrap();
        let (sender, lines) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                if sender.send(line.unwrap()).is_err() {
                    break;
                }
            }
        });
        Self { child, lines }
    }
    fn authorization_url(&self) -> String {
        loop {
            let line = self
                .lines
                .recv_timeout(Duration::from_secs(20))
                .expect("CLI did not print a login link");
            if line.starts_with("http") {
                return line;
            }
        }
    }
    fn finish(&mut self) {
        loop {
            let line = self
                .lines
                .recv_timeout(Duration::from_secs(20))
                .expect("CLI did not finish login");
            if line.starts_with("Logged in") {
                break;
            }
        }
        assert!(self.child.wait().unwrap().success());
    }
}

#[test]
fn cli_login_accepts_a_pasted_callback_and_logout_revokes_the_token() {
    let fixture = Fixture::new();
    let server = AuthorizationServer::start();
    fs::create_dir_all(fixture.root.join("config")).unwrap();
    fs::write(
        fixture.root.join("config/settings.json"),
        json!({"mcpServers":{"remote":{"url":server.resource()}}}).to_string(),
    )
    .unwrap();
    let mut login = Login::start(&fixture, "remote");
    let callback = server.approve(&login.authorization_url());
    writeln!(login.child.stdin.as_mut().unwrap(), "{callback}").unwrap();
    login.finish();
    let path = fixture.root.join("config/credentials/_mcp/remote.json");
    let saved: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    assert!(saved["token"]["access_token"].is_string());
    assert_eq!(saved["server_url"], server.resource());
    let output = succeeded(&fixture.tickloop(&["mcp", "logout", "remote"]));
    assert!(output.contains("Logged out"), "{output}");
    let saved: Value = serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
    assert!(saved.get("token").is_none());
    assert!(saved["registration"]["client_id"].is_string());
    assert_eq!(server.requests_to(REVOCATION_PATH).len(), 1);
}

#[test]
fn plugin_mcp_login_accepts_aliases_and_a_browser_callback_with_configured_oauth() {
    use std::{
        io::Read,
        net::{TcpListener, TcpStream},
    };
    use tick_oauth_test_server::{METADATA_PATH, REGISTRATION_PATH};
    let fixture = Fixture::new();
    let server = AuthorizationServer::start();
    let port = TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let source = fixture.root.join("source");
    fs::create_dir_all(source.join(".claude-plugin")).unwrap();
    fs::write(source.join(".claude-plugin/plugin.json"), json!({"name":"notes", "userConfig":{"address":{"type":"string","default":server.resource()},"client":{"type":"string","default":"configured-${LITERAL}"},"callback_port":{"type":"number","default":port},"metadata":{"type":"string","default":format!("{}{METADATA_PATH}",server.issuer())}},"mcpServers":{"remote":{"url":"${user_config.address}","oauth":{"clientId":"${user_config.client}","callbackPort":"${user_config.callback_port}"},"authServerMetadataUrl":"${user_config.metadata}"}}}).to_string()).unwrap();
    succeeded(&fixture.tickloop(&["plugin", "install", "--path", source.to_str().unwrap()]));
    let hint = fixture.tickloop(&["plugin", "login", "notes"]);
    assert!(String::from_utf8_lossy(&hint.stderr).contains("tickloop mcp login"));
    let mut login = Login::start(&fixture, "plugin:notes:remote");
    let callback = server.approve(&login.authorization_url());
    let (host, path) = callback
        .strip_prefix("http://")
        .unwrap()
        .split_once('/')
        .unwrap();
    let mut browser = TcpStream::connect(host).unwrap();
    write!(
        browser,
        "GET /{path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n"
    )
    .unwrap();
    let mut response = String::new();
    browser.read_to_string(&mut response).unwrap();
    assert!(response.starts_with("HTTP/1.1 200"));
    login.finish();
    let path = fixture
        .root
        .join("config/credentials/notes/mcp/remote.json");
    let saved: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    assert_eq!(saved["registration"]["client_id"], "configured-${LITERAL}");
    assert_eq!(
        saved["registration"]["redirect_uri"],
        format!("http://localhost:{port}/callback")
    );
    assert!(server.requests_to(REGISTRATION_PATH).is_empty());
    succeeded(&fixture.tickloop(&["mcp", "logout", "plugin_notes_remote"]));
    succeeded(&fixture.tickloop(&["plugin", "uninstall", "notes"]));
    assert!(path.exists());
}

#[test]
fn cancelling_a_cli_login_preserves_the_previous_credential() {
    let fixture = Fixture::new();
    let server = AuthorizationServer::start();
    fs::create_dir_all(fixture.root.join("config")).unwrap();
    fs::write(
        fixture.root.join("config/settings.json"),
        json!({"mcpServers":{"remote":{"url":server.resource()}}}).to_string(),
    )
    .unwrap();
    let mut login = Login::start(&fixture, "remote");
    let callback = server.approve(&login.authorization_url());
    writeln!(login.child.stdin.as_mut().unwrap(), "{callback}").unwrap();
    login.finish();
    let path = fixture.root.join("config/credentials/_mcp/remote.json");
    let before = fs::read(&path).unwrap();
    let mut cancelled = Login::start(&fixture, "remote");
    cancelled.authorization_url();
    // SAFETY: The child is owned by this test and still waiting for its callback.
    assert_eq!(
        unsafe { libc::kill(cancelled.child.id() as libc::pid_t, libc::SIGINT) },
        0
    );
    assert!(!cancelled.child.wait().unwrap().success());
    assert_eq!(fs::read(path).unwrap(), before);
}

#[test]
fn authorization_headers_do_not_start_oauth() {
    let fixture = Fixture::new();
    let server = AuthorizationServer::start();
    fs::create_dir_all(fixture.root.join("config")).unwrap();
    fs::write(fixture.root.join("config/settings.json"), json!({"mcpServers":{"remote":{"url":server.resource(),"headers":{"aUtHoRiZaTiOn":"Bearer fixed-secret"}}}}).to_string()).unwrap();
    for command in ["login", "logout"] {
        let output = fixture.tickloop(&["mcp", command, "remote"]);
        assert!(!output.status.success());
        assert!(String::from_utf8_lossy(&output.stderr).contains("Authorization header"));
        assert!(!String::from_utf8_lossy(&output.stderr).contains("fixed-secret"));
    }
    assert!(server.requests().is_empty());
    assert!(!fixture.root.join("config/credentials").exists());
}

#[test]
fn terminal_callback_input_supports_the_external_editor() {
    let fixture = Fixture::new();
    let server = AuthorizationServer::start();
    fs::create_dir_all(fixture.root.join("config")).unwrap();
    fs::write(
        fixture.root.join("config/settings.json"),
        json!({"mcpServers":{"remote":{"url":server.resource()}}}).to_string(),
    )
    .unwrap();
    let editor = fixture.root.join("editor.sh");
    fs::write(
        &editor,
        format!(
            "#!/bin/sh\ncat '{}' > \"$1\"\n",
            fixture.root.join("callback.txt").display()
        ),
    )
    .unwrap();
    let visual = format!("/bin/sh {}", editor.display());
    let mut terminal =
        fixture.tickloop_on_terminal_with_env(&["mcp", "login", "remote"], &[("VISUAL", &visual)]);
    let output = terminal.wait_for("Callback input ready");
    let url = output
        .lines()
        .find(|line| line.starts_with("http"))
        .unwrap();
    fs::write(fixture.root.join("callback.txt"), server.approve(url)).unwrap();
    terminal.type_text("\x07");
    terminal.wait_for("Callback updated; press Enter to submit");
    terminal.type_text("\r");
    terminal.wait_for("Logged in to MCP remote");
    let (code, _) = terminal.finish();
    assert_eq!(code, 0);
}

#[test]
fn a_browser_callback_closes_the_editor_owned_by_the_login() {
    use std::{io::Read, net::TcpStream, time::Instant};
    let fixture = Fixture::new();
    let server = AuthorizationServer::start();
    fs::create_dir_all(fixture.root.join("config")).unwrap();
    fs::write(
        fixture.root.join("config/settings.json"),
        json!({"mcpServers":{"remote":{"url":server.resource()}}}).to_string(),
    )
    .unwrap();
    let editor = fixture.root.join("editor.sh");
    let pid_path = fixture.root.join("editor.pid");
    fs::write(
        &editor,
        format!(
            "#!/bin/sh\necho $$ > '{}'\nexec /bin/sleep 60\n",
            pid_path.display()
        ),
    )
    .unwrap();
    let visual = format!("/bin/sh {}", editor.display());
    let mut terminal =
        fixture.tickloop_on_terminal_with_env(&["mcp", "login", "remote"], &[("VISUAL", &visual)]);
    let output = terminal.wait_for("Callback input ready");
    let url = output
        .lines()
        .find(|line| line.starts_with("http"))
        .unwrap();
    let callback = server.approve(url);
    terminal.type_text("\x07");
    let deadline = Instant::now() + Duration::from_secs(5);
    let pid = loop {
        if let Ok(text) = fs::read_to_string(&pid_path)
            && let Ok(pid) = text.trim().parse::<libc::pid_t>()
        {
            break pid;
        }
        assert!(Instant::now() < deadline, "editor did not start");
        std::thread::sleep(Duration::from_millis(10));
    };
    let (host, path) = callback
        .strip_prefix("http://")
        .unwrap()
        .split_once('/')
        .unwrap();
    let mut browser = TcpStream::connect(host).unwrap();
    write!(
        browser,
        "GET /{path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n"
    )
    .unwrap();
    let mut response = String::new();
    browser.read_to_string(&mut response).unwrap();
    assert!(response.starts_with("HTTP/1.1 200"));
    terminal.wait_for("Logged in to MCP remote");
    assert_eq!(terminal.finish().0, 0);
    // SAFETY: Signal zero observes the specific editor PID recorded by this test.
    assert_eq!(
        unsafe { libc::kill(pid, 0) },
        -1,
        "the login left its editor running"
    );
    assert_eq!(
        std::io::Error::last_os_error().raw_os_error(),
        Some(libc::ESRCH)
    );
}

#[test]
fn sensitive_plugin_values_are_not_printed_in_the_authorization_link() {
    use std::io::Read;
    let fixture = Fixture::new();
    let server = AuthorizationServer::start();
    let source = fixture.root.join("source");
    fs::create_dir_all(source.join(".claude-plugin")).unwrap();
    let secret = "private-client +/值";
    fs::write(source.join(".claude-plugin/plugin.json"),json!({"name":"private-client","userConfig":{"client":{"type":"string","sensitive":true,"default":secret}},"mcpServers":{"remote":{"url":server.resource(),"oauth":{"clientId":"${user_config.client}"}}}}).to_string()).unwrap();
    succeeded(&fixture.tickloop(&["plugin", "install", "--path", source.to_str().unwrap()]));
    let mut login = Login::start(&fixture, "plugin:private-client:remote");
    assert!(
        matches!(
            login.lines.recv_timeout(Duration::from_secs(5)),
            Err(mpsc::RecvTimeoutError::Disconnected)
        ),
        "the CLI printed a login link containing private configuration"
    );
    assert!(!login.child.wait().unwrap().success());
    let mut error = String::new();
    login
        .child
        .stderr
        .take()
        .unwrap()
        .read_to_string(&mut error)
        .unwrap();
    assert!(error.contains("sensitive plugin configuration"), "{error}");
    assert!(!error.contains(secret));
    assert!(
        !fixture
            .root
            .join("config/credentials/private-client/mcp/remote.json")
            .exists()
    );
}

#[test]
fn a_private_resource_path_keeps_literal_plus_during_nested_link_decoding() {
    let fixture = Fixture::new();
    let server = AuthorizationServer::start();
    let inner = format!(
        "https://example.test/private+{}",
        percent_encoding::utf8_percent_encode("值", percent_encoding::NON_ALPHANUMERIC)
    );
    let nested = percent_encoding::utf8_percent_encode(&inner, percent_encoding::NON_ALPHANUMERIC)
        .to_string();
    server.edit_metadata(|metadata| {
        metadata["authorization_endpoint"] =
            json!(format!("{}/authorize?continue={nested}", server.issuer()));
    });
    let source = fixture.root.join("source");
    fs::create_dir_all(source.join(".claude-plugin")).unwrap();
    fs::write(source.join(".claude-plugin/plugin.json"),json!({"name":"private-resource","userConfig":{"key":{"type":"string","sensitive":true,"default":"private+值"}},"mcpServers":{"remote":{"url":server.resource(),"headers":{"X-Api-Key":"${user_config.key}"},"oauth":{"clientId":"public-client"}}}}).to_string()).unwrap();
    succeeded(&fixture.tickloop(&["plugin", "install", "--path", source.to_str().unwrap()]));
    let mut login = Login::start(&fixture, "plugin:private-resource:remote");
    assert!(
        matches!(
            login.lines.recv_timeout(Duration::from_secs(5)),
            Err(mpsc::RecvTimeoutError::Disconnected)
        ),
        "the private resource path appeared in the printed authorization URL"
    );
    assert!(!login.child.wait().unwrap().success());
}
