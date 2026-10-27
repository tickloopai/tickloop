//! The Marketplaces page of `/plugins` in the Agent's terminal, against a private config root.
//! A local directory and a `git init` repository read through a `file://` URL stand in for
//! marketplaces; the built-in Git marketplace has no stand-in, so it cannot be read. Nothing
//! reaches the network.

use std::{fs, os::unix::fs::PermissionsExt};

use serde_json::json;
use tickloop_harness::test_support::screen;

use crate::{
    marketplace_commands::{commit_all, file_url, marketplace_directory, write_json},
    tick_package_commands::{
        Fixture, OFFICIAL_MARKETPLACE_REPOSITORY, marketplace_json, path_text, succeeded,
    },
    ticks_panel_pty::{ESCAPE, Terminal},
};

const TAB: &[u8] = b"\t";
const BACK_TAB: &[u8] = b"\x1b[Z";
const CONTROL_G: &[u8] = b"\x07";

/// The frame without its whitespace and selection marks, so text the panel wrapped still reads
/// as one.
fn compact(frame: &str) -> String {
    frame
        .chars()
        .filter(|character| !character.is_whitespace() && *character != screen::SELECTED_MARK)
        .collect()
}

fn shows(frame: &str, text: &str) -> bool {
    compact(frame).contains(&compact(text))
}

fn on_marketplaces_page(frame: &str) -> bool {
    frame
        .replace(screen::SELECTED_MARK, " ")
        .contains("Plugins · Installed  Available  [Marketplaces]")
}

/// Whether the selection fill marks the row of `name`.
fn is_selected(frame: &str, name: &str) -> bool {
    frame.lines().any(|line| {
        line.strip_prefix(&format!("◉{}{name}", screen::SELECTED_MARK))
            .is_some_and(|rest| rest.trim().is_empty() || rest.starts_with(' '))
    })
}

fn panel_is_open(frame: &str) -> bool {
    frame.contains("Plugins ·") || frame.contains("Ticks ·")
}

#[test]
fn a_built_in_git_marketplace_fetched_on_opening_shows_its_commit() {
    let fixture = Fixture::new();
    let official = fixture.root.join(OFFICIAL_MARKETPLACE_REPOSITORY);
    write_json(
        &official.join(".claude-plugin/marketplace.json"),
        &json!({"name": "claude-plugins-official", "plugins": []}),
    );
    let commit = commit_all(&official, "official");
    let marketplace = fixture.marketplace_file("empty", &marketplace_json(json!([])));
    let mut terminal = Terminal::start(&fixture, &marketplace);
    terminal.wait_for_frame(0, "the prompt", |frame| frame.contains("❯"));
    terminal.press(b"/plugins\r", "the Plugins panel", panel_is_open);
    terminal.press(BACK_TAB, "the fetched commit", |frame| {
        on_marketplaces_page(frame)
            && shows(
                frame,
                "https://github.com/anthropics/claude-plugins-official.git · built in; updated",
            )
            && shows(frame, &format!(" at {}", &commit[..12]))
    });
    terminal.press(ESCAPE, "closed", |frame| !panel_is_open(frame));
    terminal.exit();
}

#[test]
fn marketplaces_are_added_updated_and_removed_in_the_plugins_panel() {
    let fixture = Fixture::new();
    let team = marketplace_directory(
        &fixture,
        "team",
        "team",
        json!([{"name":"notes", "source":"./notes", "version":"1.0.0", "description":"Team notes"}]),
    );
    write_json(
        &team.join("notes/.claude-plugin/plugin.json"),
        &json!({"name":"notes", "version":"1.0.0"}),
    );
    let tools = marketplace_directory(
        &fixture,
        "tools",
        "tools",
        json!([{"name":"lint", "source":"./lint", "description":"Lints"}]),
    );
    write_json(
        &tools.join("lint/.claude-plugin/plugin.json"),
        &json!({"name":"lint"}),
    );
    let first_commit = commit_all(&tools, "first");
    let tools_url = file_url(&tools);
    let editor = fixture.root.join("filter-editor");
    fs::write(
        &editor,
        format!("#!/bin/sh\nprintf '%s\\n' '{tools_url}' > \"$1\"\n"),
    )
    .unwrap();
    fs::set_permissions(&editor, fs::Permissions::from_mode(0o700)).unwrap();
    let marketplace = fixture.marketplace_file("empty", &marketplace_json(json!([])));
    let mut terminal = Terminal::start(&fixture, &marketplace);
    terminal.wait_for_frame(0, "the prompt", |frame| frame.contains("❯"));
    terminal.press(b"/plugins\r", "the Plugins panel", panel_is_open);

    let frame = terminal.press(BACK_TAB, "the Marketplaces page", |frame| {
        on_marketplaces_page(frame)
            && shows(frame, "cannot read marketplace claude-plugins-official")
    });
    assert!(
        shows(
            &frame,
            &format!("◉ tickloop {marketplace} · built in; read on every use")
        ),
        "{frame}"
    );
    assert!(
        shows(
            &frame,
            "https://github.com/anthropics/claude-plugins-official.git · built in; not fetched yet"
        ),
        "{frame}"
    );
    assert!(frame.contains("enter update · a add · x remove"), "{frame}");

    terminal.press(b"x", "a built-in marketplace stays", |frame| {
        shows(frame, "The built-in marketplace tickloop cannot be removed")
            && !frame.contains("Press x again")
    });

    terminal.press(b"a", "the source field", |frame| {
        frame.contains("Add marketplace:")
            && frame.contains("enter add · ctrl+g editor · esc cancel")
    });
    terminal.press(b"jk", "list keys are text in the field", |frame| {
        frame.contains("Add marketplace: jk")
    });
    terminal.press(ESCAPE, "cancelled the field", |frame| {
        !frame.contains("Add marketplace:") && frame.contains("a add")
    });
    terminal.press(b"anot a source\r", "a source that is not one", |frame| {
        shows(
            frame,
            "Cannot add marketplace not a source: \"not a source\" is not a marketplace path, URL or GitHub repository",
        )
    });

    let team_text = path_text(&team);
    terminal.press(
        format!("a{team_text}\r").as_bytes(),
        "the directory marketplace added",
        |frame| {
            shows(
                frame,
                &format!("Added marketplace team from {team_text}; it offers 1 plugin"),
            ) && shows(frame, &format!("◉ team {team_text} · updated"))
        },
    );

    terminal.press(b"a", "the source field again", |frame| {
        frame.contains("Add marketplace:")
    });
    terminal.press(CONTROL_G, "the source from the external editor", |frame| {
        shows(frame, &format!("Add marketplace: {tools_url}"))
    });
    terminal.press(b"\r", "the Git marketplace added", |frame| {
        shows(
            frame,
            &format!("Added marketplace tools from {tools_url}; it offers 1 plugin"),
        ) && shows(frame, &format!(" at {}", &first_commit[..12]))
    });

    terminal.press(TAB, "the Installed page", |frame| {
        frame.contains("[Installed]")
    });
    terminal.press(TAB, "the added marketplaces offer their plugins", |frame| {
        frame.contains("[Available]")
            && shows(frame, "◉ lint · Lints tools")
            && shows(frame, "◉ notes · Team notes team · 1.0.0 · not installed")
    });
    terminal.press(b"/notes\r", "notes installed from team", |frame| {
        shows(frame, "Installed plugin notes@team 1.0.0")
    });
    terminal.press(ESCAPE, "left the filter", |frame| {
        frame.contains("/ filter")
    });

    write_json(
        &tools.join("review/.claude-plugin/plugin.json"),
        &json!({"name":"review"}),
    );
    write_json(
        &tools.join(".claude-plugin/marketplace.json"),
        &json!({"name": "tools", "plugins": [
            {"name":"lint", "source":"./lint", "description":"Lints"},
            {"name":"review", "source":"./review", "description":"Reviews"},
        ]}),
    );
    let second_commit = commit_all(&tools, "second");
    terminal.press(TAB, "back on the Marketplaces page", on_marketplaces_page);
    let frame = terminal.press(b"jjj", "tools selected", |frame| {
        is_selected(frame, "tools")
    });
    assert!(!shows(&frame, &second_commit[..12]), "{frame}");
    terminal.press(b"\r", "tools updated", |frame| {
        shows(frame, "Updated marketplace tools; it offers 2 plugins")
            && shows(frame, &format!(" at {}", &second_commit[..12]))
    });

    terminal.press(b"k", "team selected", |frame| is_selected(frame, "team"));
    terminal.press(b"x", "removal armed", |frame| {
        frame.contains("Press x again to remove marketplace team")
            && frame.contains("Removing a marketplace keeps the plugins installed from it.")
    });
    terminal.press(b"x", "team removed", |frame| {
        shows(
            frame,
            "Removed marketplace team; its plugins stay installed: notes@team",
        ) && !frame.contains("◉ team")
    });
    terminal.press(TAB, "the Installed page", |frame| {
        frame.contains("[Installed]") && is_selected(frame, "notes")
    });
    terminal.press(ESCAPE, "closed", |frame| !panel_is_open(frame));

    terminal.press(b"/ticks\r", "the Ticks panel", panel_is_open);
    let frame = terminal.press(TAB, "the Ticks Available page", |frame| {
        frame.contains("[Available]") && frame.contains("Manage marketplaces in /plugins")
    });
    let header = frame.lines().find(|line| line.contains("Ticks ·")).unwrap();
    assert!(!header.contains("Marketplaces"), "{frame}");
    terminal.press(ESCAPE, "closed Ticks", |frame| !panel_is_open(frame));
    terminal.exit();

    let listed =
        succeeded(&fixture.tickloop_from(&marketplace, &["plugin", "marketplace", "list"]));
    let names = listed
        .lines()
        .filter_map(|line| line.split_whitespace().next())
        .collect::<Vec<_>>();
    assert_eq!(
        names,
        ["tickloop", "claude-plugins-official", "tools"],
        "{listed}"
    );
    assert!(succeeded(&fixture.tickloop(&["plugin", "list"])).contains("notes@team"));
}
