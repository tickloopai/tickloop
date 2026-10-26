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

/// Makes the repository that stands in for the built-in `claude-plugins-official` list `plugins`.
fn official_marketplace(fixture: &Fixture, plugins: Value) -> PathBuf {
    let root = fixture.root.join(OFFICIAL_MARKETPLACE_REPOSITORY);
    write_json(
        &root.join(".claude-plugin/marketplace.json"),
        &json!({"name": "claude-plugins-official", "plugins": plugins}),
    );
    commit_all(&root, "official");
    root
}

fn cached_root(fixture: &Fixture, marketplace: &str, name: &str, revision: &str) -> String {
    path_text(
        &fixture
            .root
            .join("config/plugins/cache")
            .join(marketplace)
            .join(name)
            .join(revision),
    )
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

#[test]
fn a_plugin_installs_by_name_from_a_git_marketplace_and_updates_when_its_commit_changes() {
    let fixture = Fixture::new();
    official_marketplace(&fixture, json!([]));
    let root = marketplace_directory(
        &fixture,
        "team",
        "team-tools",
        json!([{"name": "review", "source": "./plugins/review"}]),
    );
    write_skill(&root.join("plugins/review/skills/check"), "check", "First");
    let first = commit_all(&root, "first");
    succeeded(&plugin(&fixture, &["marketplace", "add", &file_url(&root)]));
    let first_root = cached_root(&fixture, "team-tools", "review", &first[..12]);

    assert_eq!(
        succeeded(&plugin(&fixture, &["install", "review"])),
        format!(
            "Installed plugin review@team-tools {} in {first_root}.\n",
            &first[..12]
        )
    );
    assert_eq!(
        succeeded(&plugin(&fixture, &["install", "review@team-tools"])),
        format!(
            "Plugin review@team-tools {} is already installed in {first_root}; nothing changed.\n",
            &first[..12]
        )
    );
    assert!(!Path::new(&first_root).join(".git").exists());

    write_skill(&root.join("plugins/review/skills/check"), "check", "Second");
    let second = commit_all(&root, "second");
    let second_root = cached_root(&fixture, "team-tools", "review", &second[..12]);
    assert_eq!(
        succeeded(&plugin(&fixture, &["install", "review"])),
        format!(
            "Plugin review@team-tools {} is already installed in {first_root}; nothing changed.\n",
            &first[..12]
        ),
        "install reads the marketplace as last fetched"
    );
    assert_eq!(
        succeeded(&plugin(&fixture, &["update", "review"])),
        format!(
            "Updated plugin review@team-tools from {} to {} in {second_root}.\n",
            &first[..12],
            &second[..12]
        )
    );
    assert!(
        fs::read_to_string(Path::new(&second_root).join("skills/check/SKILL.md"))
            .unwrap()
            .contains("Second")
    );
    assert_eq!(
        succeeded(&plugin(&fixture, &["update", "review"])),
        format!(
            "Plugin review@team-tools {} is up to date.\n",
            &second[..12]
        )
    );

    let notes = fixture.root.join("sources/notes");
    write_skill(&notes.join("skills/write"), "write", "Write");
    succeeded(&plugin(
        &fixture,
        &["install", "--path", &path_text(&notes)],
    ));
    assert_eq!(
        succeeded(&plugin(&fixture, &["update"])),
        format!(
            "Plugin notes@local was installed from a local path; tickloop plugin install --path updates it.\n\
             Plugin review@team-tools {} is up to date.\n",
            &second[..12]
        )
    );
    assert_eq!(
        failed(&plugin(&fixture, &["update", "notes"])),
        "Error: plugin notes@local was installed from a local path; tickloop plugin install --path updates it\n"
    );
    assert_eq!(
        failed(&plugin(&fixture, &["update", "absent"])),
        "Error: plugin absent is not installed\n"
    );
}

#[test]
fn a_name_several_marketplaces_offer_lists_them_and_an_unreadable_marketplace_is_only_reported() {
    let fixture = Fixture::new();
    let team = marketplace_directory(
        &fixture,
        "team",
        "team-tools",
        json!([
            {"name": "review", "source": "./plugins/review"},
            {"name": "packaged", "source": {"source": "npm", "package": "packaged"}},
        ]),
    );
    write_skill(&team.join("plugins/review/skills/check"), "check", "Team");
    commit_all(&team, "team");
    let lab = marketplace_directory(
        &fixture,
        "lab",
        "lab",
        json!([{"name": "review", "source": "./review"}]),
    );
    write_skill(&lab.join("review/skills/check"), "check", "Lab");
    succeeded(&plugin(&fixture, &["marketplace", "add", &file_url(&team)]));
    succeeded(&plugin(&fixture, &["marketplace", "add", &path_text(&lab)]));

    let ambiguous = plugin(&fixture, &["install", "review"]);
    assert_eq!(ambiguous.status.code(), Some(1));
    assert!(
        stderr(&ambiguous).starts_with(
            "Warning: cannot read marketplace claude-plugins-official: Git operation failed: "
        ),
        "{}",
        stderr(&ambiguous)
    );
    assert!(
        stderr(&ambiguous).ends_with(
            "Error: plugin review is offered by several marketplaces: review@lab, review@team-tools; \
             choose one with tickloop plugin install review@MARKETPLACE\n"
        ),
        "{}",
        stderr(&ambiguous)
    );
    assert!(!fixture.root.join("config/plugins/installed.json").exists());

    assert_eq!(
        succeeded(&plugin(&fixture, &["install", "review@lab"])),
        format!(
            "Installed plugin review@lab local in {}.\n",
            cached_root(&fixture, "lab", "review", "local")
        )
    );
    assert_eq!(
        failed(&plugin(&fixture, &["install", "review@team-tools"])),
        "Error: plugin review is already installed from marketplace lab; uninstall it first\n"
    );
    let again = plugin(&fixture, &["install", "review"]);
    assert!(
        stdout(&again).starts_with("Reinstalled plugin review@lab local"),
        "{}",
        stdout(&again)
    );
    assert!(
        stderr(&again).starts_with("Warning: cannot read marketplace claude-plugins-official: ")
    );

    assert_eq!(
        failed(&plugin(&fixture, &["install", "packaged@team-tools"])),
        "Error: cannot install packaged@team-tools: npm sources are not supported yet\n"
    );
    assert_eq!(
        failed(&plugin(&fixture, &["install", "absent@team-tools"])),
        "Error: marketplace team-tools has no plugin absent; \
         tickloop plugin marketplace update team-tools reads it again\n"
    );
    assert_eq!(
        failed(&plugin(&fixture, &["install", "review@nowhere"])),
        "Error: marketplace nowhere is not registered\n"
    );
    assert_eq!(
        failed(&plugin(&fixture, &["install", "review@local"])),
        "Error: plugins of the local marketplace install with tickloop plugin install --path PATH\n"
    );
    let absent = plugin(&fixture, &["install", "absent"]);
    assert_eq!(absent.status.code(), Some(1));
    assert!(
        stderr(&absent).ends_with(
            "Error: no marketplace offers plugin absent; \
             tickloop plugin list --available shows what they offer\n"
        ),
        "{}",
        stderr(&absent)
    );
}
