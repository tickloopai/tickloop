//! `tickloop mcp list|add|remove` as a user runs them, against a private config root.

use std::fs;

use serde_json::{Value, json};

use crate::tick_package_commands::{Fixture, failed, path_text, succeeded};

fn settings(fixture: &Fixture) -> Value {
    serde_json::from_slice(&fs::read(fixture.root.join("config/settings.json")).unwrap()).unwrap()
}

fn mcp(fixture: &Fixture, arguments: &[&str]) -> std::process::Output {
    fixture.tickloop(&[&["mcp"][..], arguments].concat())
}

/// A plugin `docs` with one stdio server whose program would leave `started` behind if it ran.
fn install_docs_plugin(fixture: &Fixture) {
    let source = fixture.root.join("sources/docs");
    fs::create_dir_all(source.join(".claude-plugin")).unwrap();
    fs::write(
        source.join(".claude-plugin/plugin.json"),
        json!({"name":"docs"}).to_string(),
    )
    .unwrap();
    let marker = path_text(&fixture.root.join("started"));
    fs::write(
        source.join(".mcp.json"),
        json!({"search":{"command":"sh","args":["-c",format!("touch {marker}")]}}).to_string(),
    )
    .unwrap();
    succeeded(&fixture.tickloop(&["plugin", "install", "--path", source.to_str().unwrap()]));
}

#[test]
fn add_writes_the_global_or_directory_layer_and_list_starts_nothing() {
    let fixture = Fixture::new();
    assert_eq!(
        succeeded(&mcp(&fixture, &["list"])),
        "No MCP servers are configured; add one with tickloop mcp add.\n"
    );
    let added = succeeded(&mcp(
        &fixture,
        &[
            "add",
            "notes",
            "https://example.com/mcp",
            "--header",
            "X-Key: ${NOTES_KEY:-unset}",
        ],
    ));
    assert_eq!(added, "Added MCP server notes to the global settings.\n");
    let added = succeeded(&mcp(
        &fixture,
        &[
            "add",
            "local",
            "--env",
            "MODE=test",
            "--dir",
            "--",
            "python3",
            "-u",
            "server.py",
        ],
    ));
    assert_eq!(added, "Added MCP server local to this directory.\n");
    let saved = settings(&fixture);
    assert_eq!(
        saved["mcpServers"]["notes"],
        json!({"type":"http","url":"https://example.com/mcp","headers":{"X-Key":"${NOTES_KEY:-unset}"}})
    );
    assert_eq!(
        saved["projects"][path_text(&fixture.root)]["mcpServers"]["local"],
        json!({"type":"stdio","command":"python3","args":["-u","server.py"],"env":{"MODE":"test"}})
    );
    install_docs_plugin(&fixture);
    let mut disabled = settings(&fixture);
    disabled["projects"][path_text(&fixture.root)]["disabledMcpServers"] =
        json!(["plugin_docs_search"]);
    fs::write(
        fixture.root.join("config/settings.json"),
        disabled.to_string(),
    )
    .unwrap();
    assert_eq!(
        succeeded(&mcp(&fixture, &["list"])),
        "Global\n  notes · http · direct · enabled\n\
         This directory\n  local · stdio · direct · enabled\n\
         Plugin docs\n  plugin_docs_search · stdio · direct · disabled in this directory\n"
    );
    assert!(!fixture.root.join("started").exists());
}

#[test]
fn add_and_remove_refuse_conflicts_and_names_they_cannot_own() {
    let fixture = Fixture::new();
    succeeded(&mcp(&fixture, &["add", "notes", "https://example.com/mcp"]));
    for (arguments, problem) in [
        (
            &["add", "notes", "https://example.com/other"][..],
            "MCP server notes already exists in the global settings",
        ),
        (
            &["add", "tickloop-artifacts", "https://example.com/mcp"][..],
            "reserved for Tickloop Artifacts",
        ),
        (
            &["add", "plugin_notes", "https://example.com/mcp"][..],
            "plugin_ is reserved",
        ),
        (&["add", "plain", "http://example.com/mcp"][..], "HTTPS"),
        (
            &["add", "both", "https://example.com/mcp", "--", "node"][..],
            "either",
        ),
        (
            &["remove", "notes", "--dir"][..],
            "MCP server notes is not in this directory's settings; it is in the global settings",
        ),
        (
            &["remove", "plugin_docs_search"][..],
            "comes from its plugin",
        ),
    ] {
        let error = failed(&mcp(&fixture, arguments));
        assert!(error.contains(problem), "{arguments:?}: {error}");
    }
    assert_eq!(
        succeeded(&mcp(&fixture, &["remove", "notes"])),
        "Removed MCP server notes from the global settings.\n"
    );
    assert_eq!(settings(&fixture)["mcpServers"], json!({}));
}
