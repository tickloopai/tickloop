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
        "Global\n  notes · http · search · enabled\n\
         This directory\n  local · stdio · search · enabled\n\
         Plugin docs\n  plugin_docs_search · stdio · search · disabled in this directory\n"
    );
    assert!(!fixture.root.join("started").exists());
}

#[test]
fn add_writes_the_chosen_exposure_and_list_shows_it() {
    let fixture = Fixture::new();
    succeeded(&mcp(
        &fixture,
        &[
            "add",
            "notes",
            "https://example.com/mcp",
            "--exposure",
            "direct",
        ],
    ));
    succeeded(&mcp(
        &fixture,
        &[
            "add",
            "local",
            "--exposure",
            "hidden",
            "--dir",
            "--",
            "python3",
            "server.py",
        ],
    ));
    let saved = settings(&fixture);
    assert_eq!(
        saved["mcpServers"]["notes"],
        json!({"type":"http","url":"https://example.com/mcp","exposure":"direct"})
    );
    assert_eq!(
        saved["projects"][path_text(&fixture.root)]["mcpServers"]["local"],
        json!({"type":"stdio","command":"python3","args":["server.py"],"exposure":"hidden"})
    );
    assert_eq!(
        succeeded(&mcp(&fixture, &["list"])),
        "Global\n  notes · http · direct · enabled\n\
         This directory\n  local · stdio · hidden · enabled\n"
    );
}

#[test]
fn add_refuses_an_invalid_missing_or_repeated_exposure_and_writes_nothing() {
    let fixture = Fixture::new();
    succeeded(&mcp(&fixture, &["add", "notes", "https://example.com/mcp"]));
    let before = fs::read(fixture.root.join("config/settings.json")).unwrap();
    for arguments in [
        &["add", "a", "https://example.com/mcp", "--exposure", "bogus"][..],
        &[
            "add",
            "a",
            "https://example.com/mcp",
            "--exposure",
            "codemode",
        ][..],
        &["add", "a", "https://example.com/mcp", "--exposure"][..],
        &["add", "a", "--exposure", "--", "node", "server.js"][..],
        &[
            "add",
            "a",
            "https://example.com/mcp",
            "--exposure",
            "direct",
            "--exposure",
            "search",
        ][..],
        &[
            "add",
            "a",
            "--exposure",
            "hidden",
            "--exposure",
            "hidden",
            "--",
            "node",
            "server.js",
        ][..],
    ] {
        let error = failed(&mcp(&fixture, arguments));
        assert!(error.contains("--exposure"), "{arguments:?}: {error}");
        assert!(
            error.contains("Usage: tickloop mcp"),
            "{arguments:?}: {error}"
        );
        assert_eq!(
            fs::read(fixture.root.join("config/settings.json")).unwrap(),
            before,
            "{arguments:?}"
        );
    }
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
        (&["add", "team__read", "https://example.com/mcp"][..], "__"),
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

#[test]
fn a_debug_build_defaults_to_the_development_root_unless_the_config_root_is_set() {
    let fixture = Fixture::new();
    let home = fixture.root.join("home");
    let mut command = fixture.command(&["mcp", "add", "notes", "https://example.com/mcp"]);
    command.env_remove("TICKLOOP_CONFIG_DIR");
    succeeded(&command.output().unwrap());
    assert!(home.join(".tickloop-dev/settings.json").exists());
    assert!(!home.join(".tickloop").exists());

    succeeded(&mcp(&fixture, &["add", "notes", "https://example.com/mcp"]));
    assert_eq!(
        settings(&fixture)["mcpServers"]["notes"]["url"],
        "https://example.com/mcp"
    );
}
