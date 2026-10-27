//! `/mcp` in the Agent's terminal, against a private config root, with local MCP and OAuth
//! servers.

use std::fs;

use serde_json::{Value, json};
use tick_oauth_test_server::AuthorizationServer;
use tickloop_harness::test_support::{mcp_service::McpService, screen};

use crate::{
    tick_package_commands::{Fixture, marketplace_json, path_text, succeeded},
    ticks_panel_pty::{ESCAPE, Terminal, dialog_text},
};

const DOWN: &[u8] = b"j";

fn write_settings(fixture: &Fixture, settings: &Value) {
    fs::create_dir_all(fixture.root.join("config")).unwrap();
    fs::write(
        fixture.root.join("config/settings.json"),
        serde_json::to_vec_pretty(settings).unwrap(),
    )
    .unwrap();
}

fn settings(fixture: &Fixture) -> Value {
    fs::read(fixture.root.join("config/settings.json")).map_or_else(
        |_| json!({}),
        |bytes| serde_json::from_slice(&bytes).unwrap(),
    )
}

/// Whether a row shows the server `name` with `details` beside it.
fn row_shows(frame: &str, name: &str, details: &str) -> bool {
    frame.lines().any(|line| {
        let line = line.replace(screen::SELECTED_MARK, " ");
        line.contains(&format!("● {name} ")) && line.trim_end().ends_with(details)
    })
}

/// Whether the frame shows `text`, wherever the selection fill starts.
fn shows(frame: &str, text: &str) -> bool {
    frame.replace(screen::SELECTED_MARK, " ").contains(text)
}

/// The row of the server `name`, without its selection mark.
fn server_row(frame: &str, name: &str) -> Option<String> {
    frame
        .lines()
        .map(|line| line.replace(screen::SELECTED_MARK, " "))
        .find(|line| line.contains(&format!("● {name} ")))
}

fn is_selected(frame: &str, text: &str) -> bool {
    frame
        .lines()
        .any(|line| line.contains(screen::SELECTED_MARK) && line.contains(text))
}

/// Starts the Agent in the fixture's directory and gives it a Chat, which a user Shell does
/// without a model.
fn started_agent(fixture: &Fixture) -> Terminal {
    let marketplace = fixture.marketplace_file("empty", &marketplace_json(json!([])));
    let mut terminal = Terminal::start(fixture, &marketplace);
    terminal.wait_for_frame(0, "the prompt", |frame| frame.contains("❯"));
    terminal.press(b"!printf agent-started\r", "the Shell output", |frame| {
        frame.contains("⎿  agent-started")
    });
    terminal
}

#[test]
fn the_panel_groups_servers_shows_tools_reconnects_and_disables_in_this_directory() {
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let notes = runtime.block_on(McpService::start());
    notes.set_tools(
        json!([{"name":"search","description":"Find notes","inputSchema":{"type":"object"}}]),
    );
    let docs = runtime.block_on(McpService::start());
    docs.set_tools(json!([{"name":"lookup","inputSchema":{"type":"object"}}]));
    let fixture = Fixture::new();
    let plugin = fixture.root.join("sources/docs");
    fs::create_dir_all(plugin.join(".claude-plugin")).unwrap();
    fs::write(
        plugin.join(".claude-plugin/plugin.json"),
        json!({"name":"docs","mcpServers":{"search":{"url":docs.url()}}}).to_string(),
    )
    .unwrap();
    succeeded(&fixture.tickloop(&["plugin", "install", "--path", &path_text(&plugin)]));
    let mut configured = settings(&fixture);
    configured["mcpServers"] = json!({"notes":{"url":notes.url()}});
    configured["projects"] = json!({path_text(&fixture.root): {"mcpServers":{
        "broken":{"command":"tickloop-missing-mcp-program"}
    }}});
    write_settings(&fixture, &configured);
    let mut terminal = started_agent(&fixture);
    let frame = terminal.press(b"/mcp\r", "the connected servers", |frame| {
        row_shows(frame, "notes", "http · connected · 1 tool · search")
            && row_shows(
                frame,
                "plugin_docs_search",
                "http · connected · 1 tool · search",
            )
            && row_shows(frame, "broken", "stdio · failed · search")
    });
    let dialog = dialog_text(&frame);
    let order = [
        "Global",
        "notes",
        "This directory",
        "broken",
        "Plugin docs",
        "plugin_docs_search",
    ]
    .map(|text| {
        dialog
            .find(text)
            .unwrap_or_else(|| panic!("{text} in {dialog}"))
    });
    assert!(order.windows(2).all(|pair| pair[0] < pair[1]), "{dialog}");
    assert!(
        dialog.contains("enter actions · r refresh · esc close"),
        "{dialog}"
    );
    assert!(is_selected(&frame, "notes"), "{frame}");

    terminal.press(b"\r", "the actions of notes", |frame| {
        frame.contains("❯ View tools") && frame.contains("Reconnect")
    });
    terminal.press(b"\r", "the tools of notes", |frame| {
        frame.contains("mcp__notes__search · Find notes")
    });
    terminal.press(ESCAPE, "the actions again", |frame| {
        frame.contains("❯ View tools")
    });
    terminal.press(DOWN, "Log in", |frame| frame.contains("❯ Log in"));
    terminal.press(DOWN, "Reconnect", |frame| frame.contains("❯ Reconnect"));
    terminal.press(b"\r", "reconnected with its tools", |frame| {
        frame.contains("Connected to notes · 1 tool")
    });
    terminal.press(b"r", "notes connected again", |frame| {
        row_shows(frame, "notes", "http · connected · 1 tool · search")
    });

    let mark = terminal.mark();
    for _ in 0..2 {
        terminal.send(DOWN);
    }
    terminal.wait_for_frame(mark, "the plugin server selected", |frame| {
        is_selected(frame, "plugin_docs_search")
    });
    terminal.press(b"\r", "the actions of the plugin server", |frame| {
        frame.contains("plugin_docs_search") && frame.contains("Disable in this directory")
    });
    let mark = terminal.mark();
    for _ in 0..3 {
        terminal.send(DOWN);
    }
    terminal.wait_for_frame(mark, "Disable selected", |frame| {
        frame.contains("❯ Disable in this directory")
    });
    terminal.press(b"\r", "the plugin server disabled", |frame| {
        frame.contains("Disabled plugin_docs_search in this directory")
            && row_shows(frame, "plugin_docs_search", "http · disabled · search")
    });
    assert_eq!(
        settings(&fixture)["projects"][path_text(&fixture.root)]["disabledMcpServers"],
        json!(["plugin_docs_search"])
    );
    let listed = succeeded(&fixture.tickloop(&["mcp", "list"]));
    assert!(
        listed.contains("plugin_docs_search · http · search · disabled in this directory"),
        "{listed}"
    );
    terminal.press(b"\r", "the actions of the disabled server", |frame| {
        frame.contains("Enable in this directory")
    });
    terminal.press(ESCAPE, "the list", |frame| {
        frame.contains("enter actions · r refresh · esc close")
    });
    terminal.press(ESCAPE, "the panel closed", |frame| {
        !frame.contains("enter actions · r refresh")
    });
    terminal.exit();
}

/// The authorization link the login shows, its wrapped rows joined.
fn shown_link(frame: &str) -> String {
    let dialog = frame.replace(screen::SELECTED_MARK, " ");
    let rows = dialog.lines().map(str::trim).collect::<Vec<_>>();
    let start = rows
        .iter()
        .position(|row| row.starts_with("Open this link"))
        .expect("the login shows a link")
        + 1;
    rows[start..]
        .iter()
        .take_while(|row| !row.starts_with("After you approve"))
        .copied()
        .collect()
}

#[test]
fn a_login_takes_the_pasted_callback_and_saves_the_credential() {
    let server = AuthorizationServer::start();
    let fixture = Fixture::new();
    write_settings(
        &fixture,
        &json!({"mcpServers":{"remote":{"url":server.resource()}}}),
    );
    let mut terminal = started_agent(&fixture);
    terminal.press(b"/mcp\r", "the remote server", |frame| {
        shows(frame, "● remote")
    });
    terminal.press(b"\r", "its actions", |frame| frame.contains("❯ Log in"));
    let frame = terminal.press(b"\r", "the authorization link", |frame| {
        frame.contains("Open this link in a browser:")
            && frame.contains("enter submit · ctrl+g editor · esc cancel")
    });
    let link = shown_link(&frame);
    assert!(link.starts_with(server.issuer()), "{link}\n{frame}");
    let callback = server.approve(&link);
    terminal.press(b"jk", "text keys typed", |frame| frame.contains("> jk"));
    terminal.send(b"\x15");
    terminal.send(callback.as_bytes());
    // The fixture accepts the token but is no MCP server, so the connection after the login
    // fails and the panel says so.
    terminal.press(b"\r", "logged in, but not connected", |frame| {
        frame.contains("Logged in to remote, but cannot connect")
            && server_row(frame, "remote").is_some_and(|row| row.contains("failed · logged in"))
    });
    let saved: Value = serde_json::from_slice(
        &fs::read(fixture.root.join("config/credentials/_mcp/remote.json")).unwrap(),
    )
    .unwrap();
    assert!(saved["token"]["access_token"].is_string());
    terminal.press(b"\r", "the actions after login", |frame| {
        frame.contains("Log in again") && frame.contains("Log out")
    });
    terminal.press(DOWN, "Log out", |frame| frame.contains("❯ Log out"));
    terminal.press(b"\r", "logged out", |frame| {
        frame.contains("Logged out of remote")
            && server_row(frame, "remote").is_some_and(|row| !row.contains("logged in"))
    });
    let saved: Value = serde_json::from_slice(
        &fs::read(fixture.root.join("config/credentials/_mcp/remote.json")).unwrap(),
    )
    .unwrap();
    assert!(saved.get("token").is_none());

    terminal.press(b"\r", "the actions once more", |frame| {
        frame.contains("❯ Log in")
    });
    terminal.press(b"\r", "another link", |frame| {
        frame.contains("Open this link in a browser:")
    });
    terminal.press(ESCAPE, "the login cancelled", |frame| {
        frame.contains("Cancelled the login to remote")
    });
    assert!(
        !fs::read_to_string(fixture.root.join("config/credentials/_mcp/remote.json"))
            .unwrap()
            .contains("access_token")
    );
    terminal.press(ESCAPE, "the panel closed", |frame| {
        !shows(frame, "● remote")
    });
    terminal.exit();
}

#[test]
fn before_the_agent_starts_remote_servers_connect_and_local_ones_wait_for_connect() {
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let notes = runtime.block_on(McpService::start());
    notes.set_tools(json!([{"name":"search","inputSchema":{"type":"object"}}]));
    let local = runtime.block_on(McpService::start());
    local.set_tools(json!([{"name":"lookup","inputSchema":{"type":"object"}}]));
    let fixture = Fixture::new();
    write_settings(
        &fixture,
        &json!({"mcpServers":{
            "notes":{"url":notes.url()},
            "local":{"command":"python3","args":local.stdio_args()}
        }}),
    );
    let marketplace = fixture.marketplace_file("empty", &marketplace_json(json!([])));
    let mut terminal = Terminal::start(&fixture, &marketplace);
    terminal.wait_for_frame(0, "the prompt", |frame| frame.contains("❯"));
    terminal.press(b"/mcp\r", "the remote server connected", |frame| {
        row_shows(frame, "notes", "http · connected · 1 tool · search")
            && row_shows(frame, "local", "stdio · starts with the Agent · search")
    });
    assert_eq!(local.request_count(), 0, "no local program starts unasked");
    assert!(is_selected(&terminal.frame(), "local"));
    terminal.press(b"\r", "its actions", |frame| frame.contains("❯ Connect"));
    terminal.press(b"\r", "the local server connected", |frame| {
        frame.contains("Connected to local · 1 tool")
            && row_shows(frame, "local", "stdio · connected · 1 tool · search")
    });
    terminal.press(ESCAPE, "the panel closed", |frame| {
        !frame.contains("enter actions · r refresh")
    });
    terminal.exit();
}
