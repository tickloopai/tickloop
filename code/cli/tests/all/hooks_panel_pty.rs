//! `/hooks` and the review a New Agent offers, in the Agent's terminal, against a private config
//! root whose Folder has its own hooks.

use std::{fs, thread, time::Duration};

use serde_json::{Value, json};
use tickloop_harness::test_support::screen;

use crate::{
    tick_package_commands::{Fixture, marketplace_json, path_text, succeeded},
    ticks_panel_pty::{ESCAPE, Terminal, dialog_text},
};

const CONTROL_N: &[u8] = b"\x0e";
const CONTROL_P: &[u8] = b"\x10";
const REVIEW_TITLE: &str = "Hooks need review";
const LIST_HINT: &str = "esc close";
/// How long a review that is not offered is given to show anyway.
const NO_REVIEW_WAIT: Duration = Duration::from_secs(2);

fn settings(fixture: &Fixture) -> Value {
    fs::read(fixture.root.join("config/settings.json")).map_or_else(
        |_| json!({}),
        |bytes| serde_json::from_slice(&bytes).unwrap(),
    )
}

fn write_settings(fixture: &Fixture, settings: &Value) {
    fs::create_dir_all(fixture.root.join("config")).unwrap();
    fs::write(
        fixture.root.join("config/settings.json"),
        serde_json::to_vec_pretty(settings).unwrap(),
    )
    .unwrap();
}

/// The Folder's trust records, as the user's settings keep them.
fn folder_trust(fixture: &Fixture) -> Value {
    settings(fixture)["projects"][path_text(&fixture.root)]["hookTrust"].clone()
}

fn command_hook(command: &str) -> Value {
    json!({"type": "command", "command": command})
}

/// A user hook, and two Folder hooks with a script in `.claude/hooks/`.
fn configure_hooks(fixture: &Fixture) {
    let mut configured = settings(fixture);
    configured["hooks"] = json!({"PreToolUse": [{
        "matcher": "Bash",
        "hooks": [command_hook("echo user-guard")],
    }]});
    write_settings(fixture, &configured);
    let claude = fixture.root.join(".claude");
    fs::create_dir_all(claude.join("hooks")).unwrap();
    fs::write(claude.join("hooks/format.sh"), "#!/bin/sh\nexit 0\n").unwrap();
    fs::write(
        claude.join("settings.json"),
        json!({"hooks": {
            "PostToolUse": [{
                "matcher": "Edit|Write",
                "hooks": [command_hook("./.claude/hooks/format.sh")],
            }],
            "Stop": [{"hooks": [command_hook("echo repo-stop")]}],
        }})
        .to_string(),
    )
    .unwrap();
}

/// Installs a plugin whose `hooks/hooks.json` declares one hook.
fn install_plugin_with_hooks(fixture: &Fixture) {
    let plugin = fixture.root.join("sources/guard");
    fs::create_dir_all(plugin.join(".claude-plugin")).unwrap();
    fs::create_dir_all(plugin.join("hooks")).unwrap();
    fs::write(
        plugin.join(".claude-plugin/plugin.json"),
        json!({"name": "guard"}).to_string(),
    )
    .unwrap();
    fs::write(
        plugin.join("hooks/hooks.json"),
        json!({"hooks": {"SessionStart": [{
            "matcher": "startup",
            "hooks": [command_hook("echo plugin-start")],
        }]}})
        .to_string(),
    )
    .unwrap();
    succeeded(&fixture.tickloop(&["plugin", "install", "--path", &path_text(&plugin)]));
}

fn start(fixture: &Fixture) -> Terminal {
    let marketplace = fixture.marketplace_file("empty", &marketplace_json(json!([])));
    Terminal::start(fixture, &marketplace)
}

fn shows(frame: &str, text: &str) -> bool {
    frame.replace(screen::SELECTED_MARK, " ").contains(text)
}

fn is_selected(frame: &str, text: &str) -> bool {
    frame
        .lines()
        .any(|line| line.contains(screen::SELECTED_MARK) && line.contains(text))
}

/// Whether the row that shows `text` ends with the trust `state`; `false` while no row shows it,
/// as before the list has loaded.
fn row_state(frame: &str, text: &str, state: &str) -> bool {
    frame
        .lines()
        .map(|line| line.replace(screen::SELECTED_MARK, " "))
        .find(|line| line.contains(text))
        .is_some_and(|line| line.trim_end().ends_with(state))
}

/// The key hints, or what replaces them: the dialog's last row.
fn hint_line(frame: &str) -> String {
    frame
        .lines()
        .rev()
        .find(|line| !line.trim().is_empty())
        .unwrap_or_default()
        .replace(screen::SELECTED_MARK, " ")
}

fn wait_for_review(terminal: &Terminal) -> String {
    terminal.wait_for_frame(0, "the hooks review", |frame| {
        shows(frame, REVIEW_TITLE) && shows(frame, "❯ Review them one by one")
    })
}

#[test]
fn reviewing_one_by_one_opens_the_list_where_hooks_are_trusted_disabled_and_restored() {
    let fixture = Fixture::new();
    install_plugin_with_hooks(&fixture);
    configure_hooks(&fixture);
    let mut terminal = start(&fixture);
    let review = wait_for_review(&terminal);
    let dialog = dialog_text(&review);
    assert!(
        dialog.contains("3 hooks in this folder are new or changed."),
        "{dialog}"
    );
    assert!(dialog.contains("Trust all and continue"), "{dialog}");
    assert!(dialog.contains("Continue without trusting"), "{dialog}");
    assert!(dialog.contains("enter confirm · esc skip"), "{dialog}");

    let frame = terminal.press(b"\r", "the hooks list", |frame| {
        shows(frame, LIST_HINT) && shows(frame, "Plugin guard@")
    });
    let dialog = dialog_text(&frame);
    let order = [
        "Your settings",
        "PreToolUse Bash · echo user-guard",
        "This folder",
        "PostToolUse Edit|Write · ./.claude/hooks/format.sh",
        "Stop · echo repo-stop",
        "Plugin guard@",
        "SessionStart startup · echo plugin-start",
    ]
    .map(|text| {
        dialog
            .find(text)
            .unwrap_or_else(|| panic!("{text} in {dialog}"))
    });
    assert!(order.windows(2).all(|pair| pair[0] < pair[1]), "{dialog}");
    assert!(row_state(&frame, "echo user-guard", "trusted"));
    assert!(row_state(&frame, "format.sh", "pending"));
    assert!(
        is_selected(&frame, "format.sh"),
        "the first hook to review is selected:\n{frame}"
    );
    assert!(!dialog.contains('❯'), "list rows carry no cursor: {dialog}");
    assert!(
        hint_line(&frame)
            .contains("enter details · space trust · x disable · r refresh · esc close"),
        "{frame}"
    );

    terminal.press(b"\r", "the details", |frame| {
        shows(frame, "Source: This folder") && shows(frame, "esc back")
    });
    let details = dialog_text(&terminal.frame());
    assert!(
        details.contains("Folder's .claude/hooks/ directory"),
        "{details}"
    );
    assert!(details.contains("are not covered"), "{details}");
    terminal.press(ESCAPE, "the list again", |frame| shows(frame, LIST_HINT));

    terminal.press(b" ", "the hook trusted", |frame| {
        shows(frame, "Trusted PostToolUse Edit|Write") && row_state(frame, "format.sh", "trusted")
    });
    let trust = folder_trust(&fixture);
    assert_eq!(
        trust.as_object().map(|records| records.len()),
        Some(1),
        "{trust}"
    );

    let frame = terminal.press(b"x", "the confirmation", |frame| {
        hint_line(frame).contains("Press x again to disable PostToolUse Edit|Write")
    });
    assert!(row_state(&frame, "format.sh", "trusted"));
    terminal.press(CONTROL_N, "the confirmation cancelled by moving", |frame| {
        is_selected(frame, "repo-stop") && hint_line(frame).contains(LIST_HINT)
    });
    terminal.press(CONTROL_P, "the trusted hook again", |frame| {
        is_selected(frame, "format.sh")
    });
    terminal.press(b"x", "the confirmation again", |frame| {
        hint_line(frame).contains("Press x again")
    });
    terminal.press(b"z", "the confirmation cancelled by another key", |frame| {
        hint_line(frame).contains(LIST_HINT)
    });
    assert!(row_state(&terminal.frame(), "format.sh", "trusted"));

    terminal.send(b"x");
    terminal.press(b"x", "the hook disabled", |frame| {
        shows(frame, "Disabled PostToolUse Edit|Write")
            && row_state(frame, "format.sh", "disabled")
            && hint_line(frame).contains("x restore")
    });
    terminal.send(b"x");
    terminal.press(b"x", "the hook restored", |frame| {
        shows(frame, "Restored PostToolUse Edit|Write") && row_state(frame, "format.sh", "trusted")
    });
    terminal.press(ESCAPE, "the panel closed", |frame| !shows(frame, LIST_HINT));

    // A New Agent in the same Folder does not offer the review again in this Console.
    terminal.press(b"!printf agent-started\r", "the Shell output", |frame| {
        frame.contains("⎿  agent-started")
    });
    terminal.press(b"/clear\r", "the New Agent", |frame| {
        !frame.contains("⎿  agent-started")
    });
    thread::sleep(NO_REVIEW_WAIT);
    assert!(
        !shows(&terminal.frame(), REVIEW_TITLE),
        "{}",
        terminal.frame()
    );
    terminal.exit();
}

#[test]
fn trusting_all_from_the_review_trusts_every_hook_to_review() {
    let fixture = Fixture::new();
    configure_hooks(&fixture);
    let mut terminal = start(&fixture);
    wait_for_review(&terminal);
    terminal.press(CONTROL_N, "trust all selected", |frame| {
        shows(frame, "❯ Trust all and continue")
    });
    terminal.press(b"\r", "the review closed", |frame| {
        !shows(frame, REVIEW_TITLE) && frame.contains('❯')
    });
    let trust = folder_trust(&fixture);
    assert_eq!(
        trust.as_object().map(|records| records.len()),
        Some(2),
        "{trust}"
    );
    terminal.press(b"/hooks\r", "every hook trusted", |frame| {
        shows(frame, LIST_HINT)
            && row_state(frame, "format.sh", "trusted")
            && row_state(frame, "repo-stop", "trusted")
    });
    terminal.press(ESCAPE, "the panel closed", |frame| !shows(frame, LIST_HINT));
    terminal.exit();
}

#[test]
fn continuing_without_trusting_leaves_the_hooks_pending() {
    let fixture = Fixture::new();
    configure_hooks(&fixture);
    let mut terminal = start(&fixture);
    wait_for_review(&terminal);
    terminal.send(b"\x1b[B");
    terminal.press(b"\x1b[B", "continue selected", |frame| {
        shows(frame, "❯ Continue without trusting")
    });
    terminal.press(b"\r", "the review closed", |frame| {
        !shows(frame, REVIEW_TITLE)
    });
    assert!(folder_trust(&fixture).is_null(), "{}", settings(&fixture));
    terminal.press(b"/hooks\r", "the hooks still pending", |frame| {
        shows(frame, LIST_HINT)
            && row_state(frame, "format.sh", "pending")
            && row_state(frame, "repo-stop", "pending")
    });
    terminal.press(ESCAPE, "the panel closed", |frame| !shows(frame, LIST_HINT));
    terminal.exit();
}
