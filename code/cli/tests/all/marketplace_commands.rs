//! `tickloop plugin marketplace …`, and installing and updating plugins by name, as a user runs
//! them against a private config root. Local `git init` repositories and directories stand in for
//! marketplaces and their plugin sources; nothing reaches the network.

use std::{
    fs,
    path::{Path, PathBuf},
    process::Output,
};

use serde_json::{Value, json};

use crate::tick_package_commands::{
    Fixture, OFFICIAL_MARKETPLACE_REPOSITORY, failed, marketplace_json, path_text, succeeded,
};

const DEFAULT_TICK_MARKETPLACE: &str = "tick-marketplace";

fn git(root: &Path, arguments: &[&str]) -> String {
    let output = std::process::Command::new("git")
        .args([
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.test",
            "-c",
            "commit.gpgsign=false",
            "-c",
            "core.hooksPath=/dev/null",
            "-c",
            "init.defaultBranch=main",
        ])
        .args(arguments)
        .current_dir(root)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}

/// Commits everything in `root`, making it a repository first, and returns the commit.
fn commit_all(root: &Path, message: &str) -> String {
    if !root.join(".git").exists() {
        git(root, &["init"]);
    }
    git(root, &["add", "-A"]);
    git(root, &["commit", "-q", "-m", message]);
    git(root, &["rev-parse", "HEAD"])
}

fn write_skill(directory: &Path, name: &str, body: &str) {
    fs::create_dir_all(directory).unwrap();
    fs::write(
        directory.join("SKILL.md"),
        format!("---\nname: {name}\ndescription: Uses {name}\n---\n{body}\n"),
    )
    .unwrap();
}

fn write_json(path: &Path, value: &Value) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, value.to_string()).unwrap();
}

/// A directory `label` holding a Claude format marketplace called `name` listing `plugins`.
fn marketplace_directory(fixture: &Fixture, label: &str, name: &str, plugins: Value) -> PathBuf {
    let root = fixture.root.join("sources").join(label);
    write_json(
        &root.join(".claude-plugin/marketplace.json"),
        &json!({"name": name, "owner": {"name": "Test"}, "plugins": plugins}),
    );
    root
}

fn file_url(path: &Path) -> String {
    format!("file://{}", path.display())
}

/// Runs `tickloop plugin …` with the Tick marketplace read from an empty local file.
fn plugin(fixture: &Fixture, arguments: &[&str]) -> Output {
    let tick_marketplace = fixture
        .root
        .join(format!("{DEFAULT_TICK_MARKETPLACE}.json"));
    if !tick_marketplace.exists() {
        fixture.marketplace_file(DEFAULT_TICK_MARKETPLACE, &marketplace_json(json!([])));
    }
    let arguments = [&["plugin"][..], arguments].concat();
    fixture.tickloop_from(&path_text(&tick_marketplace), &arguments)
}

/// The cells of each line of a table, which separates them by at least two spaces.
fn rows(table: &str) -> Vec<Vec<String>> {
    table
        .lines()
        .map(|line| {
            line.split("  ")
                .map(str::trim)
                .filter(|cell| !cell.is_empty())
                .map(str::to_owned)
                .collect()
        })
        .collect()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

#[test]
fn the_built_in_marketplaces_are_listed_without_fetching_them_and_cannot_be_removed() {
    let fixture = Fixture::new();
    let tick_marketplace = path_text(
        &fixture
            .root
            .join(format!("{DEFAULT_TICK_MARKETPLACE}.json")),
    );

    assert_eq!(
        rows(&succeeded(&plugin(&fixture, &["marketplace", "list"]))),
        [
            vec!["tickloop", &tick_marketplace, "built in; read on every use"],
            vec![
                "claude-plugins-official",
                "https://github.com/anthropics/claude-plugins-official.git",
                "built in; not fetched yet"
            ],
        ]
    );
    assert!(!fixture.root.join("config/plugins/marketplaces").exists());
    for builtin in ["tickloop", "claude-plugins-official"] {
        assert_eq!(
            failed(&plugin(&fixture, &["marketplace", "remove", builtin])),
            format!("Error: the built-in marketplace {builtin} cannot be removed\n")
        );
    }
}

#[test]
fn a_git_marketplace_is_added_listed_updated_and_removed() {
    let fixture = Fixture::new();
    let root = marketplace_directory(
        &fixture,
        "team",
        "team-tools",
        json!([
            {"name": "review", "source": "./plugins/review", "description": "Reviews code"},
            {"name": "packaged", "source": {"source": "npm", "package": "packaged"}},
        ]),
    );
    write_skill(
        &root.join("plugins/review/skills/check"),
        "check",
        "Check it",
    );
    let first = commit_all(&root, "first");
    let url = file_url(&root);

    assert_eq!(
        succeeded(&plugin(&fixture, &["marketplace", "add", &url])),
        format!("Added marketplace team-tools from {url}; it offers 2 plugins.\n")
    );
    let listed = rows(&succeeded(&plugin(&fixture, &["marketplace", "list"])));
    assert_eq!(listed.len(), 3, "{listed:?}");
    assert_eq!(listed[2][..2], ["team-tools", url.as_str()]);
    assert!(
        listed[2][2].starts_with("updated ")
            && listed[2][2].ends_with(&format!(" at {}", &first[..12])),
        "{listed:?}"
    );
    assert_eq!(
        failed(&plugin(&fixture, &["marketplace", "add", &url])),
        format!(
            "Error: cannot add marketplace {url}: marketplace team-tools is already registered\n"
        )
    );
    assert_eq!(
        failed(&plugin(&fixture, &["marketplace", "add", "not a source"])),
        "Error: cannot add marketplace not a source: \"not a source\" is not a marketplace path, URL or GitHub repository\n"
    );

    write_skill(&root.join("plugins/lint/skills/lint"), "lint", "Lint it");
    write_json(
        &root.join(".claude-plugin/marketplace.json"),
        &json!({"name": "team-tools", "plugins": [
            {"name": "review", "source": "./plugins/review"},
            {"name": "lint", "source": "./plugins/lint"},
            {"name": "packaged", "source": {"source": "npm", "package": "packaged"}},
        ]}),
    );
    let second = commit_all(&root, "second");
    assert_eq!(
        succeeded(&plugin(&fixture, &["marketplace", "update", "team-tools"])),
        "Updated marketplace team-tools; it offers 3 plugins.\n"
    );
    let listed = rows(&succeeded(&plugin(&fixture, &["marketplace", "list"])));
    assert!(
        listed[2][2].ends_with(&format!(" at {}", &second[..12])),
        "{listed:?}"
    );
    assert_eq!(
        failed(&plugin(&fixture, &["marketplace", "update", "absent"])),
        "Error: cannot update marketplace absent: marketplace absent is not registered\n"
    );

    assert_eq!(
        succeeded(&plugin(&fixture, &["marketplace", "remove", "team-tools"])),
        "Removed marketplace team-tools.\n"
    );
    assert_eq!(
        rows(&succeeded(&plugin(&fixture, &["marketplace", "list"]))).len(),
        2
    );
    assert_eq!(
        failed(&plugin(&fixture, &["marketplace", "remove", "team-tools"])),
        "Error: marketplace team-tools is not registered\n"
    );
}
