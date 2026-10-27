//! `tickloop plugin marketplace …`, and installing and updating plugins by name, as a user runs
//! them against a private config root. Local `git init` repositories and directories stand in for
//! marketplaces and their plugin sources; nothing reaches the network.

use std::{
    fs,
    path::{Path, PathBuf},
    process::Output,
};

use serde_json::{Value, json};

use tick_packages::CURRENT_TARGET;

use crate::tick_package_commands::{
    Fixture, OFFICIAL_MARKETPLACE_REPOSITORY, Reply, Server, archive_of, failed, marketplace_json,
    offered, path_text, succeeded,
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
pub(crate) fn commit_all(root: &Path, message: &str) -> String {
    if !root.join(".git").exists() {
        git(root, &["init"]);
    }
    git(root, &["add", "-A"]);
    git(root, &["commit", "-q", "-m", message]);
    git(root, &["rev-parse", "HEAD"])
}

pub(crate) fn write_skill(directory: &Path, name: &str, body: &str) {
    fs::create_dir_all(directory).unwrap();
    fs::write(
        directory.join("SKILL.md"),
        format!("---\nname: {name}\ndescription: Uses {name}\n---\n{body}\n"),
    )
    .unwrap();
}

pub(crate) fn write_json(path: &Path, value: &Value) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, value.to_string()).unwrap();
}

/// A directory `label` holding a Claude format marketplace called `name` listing `plugins`.
pub(crate) fn marketplace_directory(
    fixture: &Fixture,
    label: &str,
    name: &str,
    plugins: Value,
) -> PathBuf {
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

pub(crate) fn file_url(path: &Path) -> String {
    format!("file://{}", path.display())
}

/// Runs `tickloop plugin …` with the Tick marketplace read from an empty local file.
fn plugin(fixture: &Fixture, arguments: &[&str]) -> Output {
    with_local_tick_marketplace(fixture, &[&["plugin"][..], arguments].concat())
}

fn tick(fixture: &Fixture, arguments: &[&str]) -> Output {
    with_local_tick_marketplace(fixture, &[&["tick"][..], arguments].concat())
}

/// Runs tickloop with `arguments` against the fixture's Tick marketplace file, an empty one unless
/// the test wrote it.
fn with_local_tick_marketplace(fixture: &Fixture, arguments: &[&str]) -> Output {
    let tick_marketplace = fixture
        .root
        .join(format!("{DEFAULT_TICK_MARKETPLACE}.json"));
    if !tick_marketplace.exists() {
        fixture.marketplace_file(DEFAULT_TICK_MARKETPLACE, &marketplace_json(json!([])));
    }
    fixture.tickloop_from(&path_text(&tick_marketplace), arguments)
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
fn the_default_marketplaces_are_listed_without_fetching_and_tickloop_cannot_be_removed() {
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
    assert_eq!(
        failed(&plugin(&fixture, &["marketplace", "remove", "tickloop"])),
        "Error: the built-in marketplace tickloop cannot be removed\n"
    );
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

    succeeded(&plugin(&fixture, &["uninstall", "review"]));
    let notes = fixture.root.join("sources/notes");
    write_skill(&notes.join("skills/write"), "write", "Write");
    succeeded(&plugin(
        &fixture,
        &["install", "--path", &path_text(&notes)],
    ));
    assert_eq!(
        succeeded(&plugin(&fixture, &["update"])),
        "Plugin notes@local was installed from a local path; tickloop plugin install --path updates it.\n",
        "updating every plugin passes over a local one even when it is the only one"
    );
    succeeded(&plugin(&fixture, &["install", "review"]));
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
            "Error: no marketplace that could be read offers plugin absent; \
             tickloop plugin list --available shows what they offer\n"
        ),
        "{}",
        stderr(&absent)
    );

    fs::remove_dir_all(cached_root(&fixture, "lab", "review", "local")).unwrap();
    let repaired = plugin(&fixture, &["install", "review"]);
    assert!(
        stdout(&repaired).contains("plugin review@lab local in"),
        "an unusable plugin is installed again from its own marketplace: {}{}",
        stdout(&repaired),
        stderr(&repaired)
    );
}

#[test]
fn plugins_of_a_removed_marketplace_stay_and_are_passed_over_when_updating_every_plugin() {
    let fixture = Fixture::new();
    official_marketplace(&fixture, json!([]));
    let root = marketplace_directory(
        &fixture,
        "team",
        "team-tools",
        json!([{"name": "review", "source": "./plugins/review"}]),
    );
    write_skill(&root.join("plugins/review/skills/check"), "check", "Check");
    commit_all(&root, "first");
    succeeded(&plugin(&fixture, &["marketplace", "add", &file_url(&root)]));
    succeeded(&plugin(&fixture, &["install", "review"]));

    assert_eq!(
        succeeded(&plugin(&fixture, &["marketplace", "remove", "team-tools"])),
        "Removed marketplace team-tools.\n\
         Its plugins stay installed: review@team-tools; tickloop plugin uninstall removes one.\n"
    );
    assert_eq!(
        succeeded(&plugin(&fixture, &["update"])),
        "Plugin review@team-tools is not updated: marketplace team-tools is not registered; \
         tickloop plugin marketplace add registers it again.\n"
    );
    assert_eq!(
        failed(&plugin(&fixture, &["update", "review"])),
        "Error: cannot update review@team-tools: marketplace team-tools is not registered\n"
    );
}

#[test]
fn listing_or_installing_by_name_fails_when_no_marketplace_can_be_read() {
    let fixture = Fixture::new();
    let missing = path_text(&fixture.root.join("missing.json"));
    for arguments in [
        &["plugin", "list", "--available"][..],
        &["plugin", "install", "review"],
    ] {
        let output = fixture.tickloop_from(&missing, arguments);
        assert_eq!(output.status.code(), Some(1), "{arguments:?}");
        let errors = stderr(&output);
        let lines = errors.lines().collect::<Vec<_>>();
        assert_eq!(lines.len(), 3, "{errors}");
        assert!(
            lines[0].starts_with("Warning: cannot read marketplace tickloop: "),
            "{errors}"
        );
        assert!(
            lines[1].starts_with("Warning: cannot read marketplace claude-plugins-official: "),
            "{errors}"
        );
        assert_eq!(lines[2], "Error: no marketplace could be read");
    }
}

#[test]
fn the_available_plugins_merge_every_marketplace_and_one_that_cannot_be_read_stops_nothing() {
    let fixture = Fixture::new();
    let other_target = "x86_64-unknown-linux-gnu";
    let sha256 = "a".repeat(64);
    fixture.marketplace_file(
        DEFAULT_TICK_MARKETPLACE,
        &marketplace_json(json!([
            offered(
                "sample",
                "1.0.0",
                &[(CURRENT_TARGET, "https://example.com/sample.tar.gz", &sha256)]
            ),
            offered(
                "elsewhere",
                "0.1.0",
                &[(
                    other_target,
                    "https://example.com/elsewhere.tar.gz",
                    &sha256
                )]
            ),
        ])),
    );
    let team = marketplace_directory(
        &fixture,
        "team",
        "team-tools",
        json!([
            {"name": "review", "source": "./plugins/review", "description": "Reviews code"},
            {"name": "packaged", "source": {"source": "npm", "package": "packaged"}},
        ]),
    );
    write_skill(&team.join("plugins/review/skills/check"), "check", "Team");
    let commit = commit_all(&team, "team");
    let lab = marketplace_directory(
        &fixture,
        "lab",
        "lab",
        json!([
            {"name": "review", "source": "./review", "version": "3.0.0"},
            {"name": "lint", "source": "./lint", "description": "Lints"},
        ]),
    );
    write_skill(&lab.join("review/skills/check"), "check", "Lab");
    write_skill(&lab.join("lint/skills/lint"), "lint", "Lint");
    succeeded(&plugin(&fixture, &["marketplace", "add", &file_url(&team)]));
    succeeded(&plugin(&fixture, &["marketplace", "add", &path_text(&lab)]));
    succeeded(&plugin(&fixture, &["install", "review@team-tools"]));
    succeeded(&plugin(&fixture, &["install", "lint@lab"]));
    for (arguments, error) in [
        (
            &["install", "lint@lab"][..],
            "Error: lint is not offered as a Tick; use tickloop plugin install lint@lab\n",
        ),
        (
            &["install", "lint"],
            "Error: lint is not offered as a Tick; use tickloop plugin install lint\n",
        ),
    ] {
        let refused = failed(&tick(&fixture, arguments));
        assert!(refused.ends_with(error), "{arguments:?}: {refused}");
    }

    let available = plugin(&fixture, &["list", "--available"]);
    assert!(available.status.success(), "{}", stderr(&available));
    let short = &commit[..12];
    assert_eq!(
        rows(&stdout(&available)),
        [
            vec![
                "lint@lab".to_owned(),
                "-".to_owned(),
                "installed local".to_owned(),
                "Lints".to_owned()
            ],
            vec![
                "packaged@team-tools".to_owned(),
                "-".to_owned(),
                "not installed; npm sources are not supported yet".to_owned(),
            ],
            vec![
                "review@lab".to_owned(),
                "3.0.0".to_owned(),
                "not installed; review@team-tools is installed".to_owned(),
            ],
            vec![
                "review@team-tools".to_owned(),
                "-".to_owned(),
                format!("installed {short}"),
                "Reviews code".to_owned(),
            ],
        ]
    );
    let ticks = tick(&fixture, &["list", "--available"]);
    assert_eq!(
        rows(&stdout(&ticks)),
        [
            vec![
                "elsewhere@tickloop".to_owned(),
                "0.1.0".to_owned(),
                format!("not installed; no prebuilt program for {CURRENT_TARGET}"),
                "The elsewhere package".to_owned(),
            ],
            vec![
                "sample@tickloop".to_owned(),
                "1.0.0".to_owned(),
                "not installed".to_owned(),
                "The sample package".to_owned(),
            ],
        ]
    );
    assert!(
        stderr(&available).starts_with(
            "Warning: cannot read marketplace claude-plugins-official: Git operation failed: "
        ),
        "{}",
        stderr(&available)
    );
    assert_eq!(stderr(&available).lines().count(), 1);

    fs::rename(&team, fixture.root.join("sources/team-moved")).unwrap();
    let updated = plugin(&fixture, &["update"]);
    assert_eq!(updated.status.code(), Some(1));
    assert_eq!(
        stdout(&updated),
        format!(
            "Reinstalled plugin lint@lab local in {}.\n",
            cached_root(&fixture, "lab", "lint", "local")
        )
    );
    let errors = stderr(&updated);
    assert!(
        errors.starts_with(
            "Error: cannot update review@team-tools: cannot read marketplace team-tools: Git operation failed: "
        ),
        "{errors}"
    );
    assert!(
        errors.ends_with("\nError: 1 of 2 plugins could not be updated\n"),
        "{errors}"
    );

    let refreshed = plugin(&fixture, &["marketplace", "update"]);
    assert_eq!(refreshed.status.code(), Some(1));
    assert_eq!(
        stdout(&refreshed),
        "Updated marketplace tickloop; it offers 2 plugins.\n\
         Updated marketplace lab; it offers 2 plugins.\n"
    );
    let errors = stderr(&refreshed)
        .lines()
        .map(str::to_owned)
        .collect::<Vec<_>>();
    assert_eq!(errors.len(), 3, "{errors:?}");
    assert!(errors[0].starts_with("Error: cannot update marketplace claude-plugins-official: "));
    assert!(errors[1].starts_with("Error: cannot update marketplace team-tools: "));
    assert_eq!(errors[2], "Error: 2 of 4 marketplaces could not be updated");
    assert_eq!(
        rows(&succeeded(&plugin(&fixture, &["list"])))
            .iter()
            .map(|row| row[0].clone())
            .collect::<Vec<_>>(),
        ["lint@lab", "review@team-tools"]
    );
}

#[test]
fn the_built_in_git_marketplace_is_fetched_on_first_use_and_installs_each_kind_of_git_source() {
    let fixture = Fixture::new();
    let external = fixture.root.join("sources/external");
    write_skill(&external.join("skills/fetch"), "fetch", "Fetch");
    let external_commit = commit_all(&external, "external");
    let monorepo = fixture.root.join("sources/monorepo");
    write_skill(&monorepo.join("plugins/nested/skills/nest"), "nest", "Nest");
    write_skill(&monorepo.join("other/skills/other"), "other", "Other");
    let monorepo_commit = commit_all(&monorepo, "monorepo");
    let official = fixture.root.join(OFFICIAL_MARKETPLACE_REPOSITORY);
    write_json(
        &official.join("plugins/notion/.claude-plugin/plugin.json"),
        &json!({"name": "Notion", "version": "1.0.0"}),
    );
    write_skill(
        &official.join("plugins/notion/skills/pages"),
        "pages",
        "Pages",
    );
    let official_commit = {
        official_marketplace(
            &fixture,
            json!([
                {"name": "notion", "source": "./plugins/notion"},
                {"name": "fetcher", "source": {"source": "url", "url": file_url(&external)}},
                {"name": "nested", "source": {
                    "source": "git-subdir", "url": file_url(&monorepo), "path": "plugins/nested"
                }},
            ]),
        );
        git(&official, &["rev-parse", "HEAD"])
    };
    let official_row = |fixture: &Fixture| {
        rows(&succeeded(&plugin(fixture, &["marketplace", "list"])))
            .into_iter()
            .find(|row| row[0] == "claude-plugins-official")
            .unwrap()
    };
    assert_eq!(official_row(&fixture)[2], "built in; not fetched yet");

    let notion_root = cached_root(
        &fixture,
        "claude-plugins-official",
        "notion",
        &official_commit[..12],
    );
    assert_eq!(
        succeeded(&plugin(&fixture, &["install", "notion"])),
        format!(
            "Installed plugin notion@claude-plugins-official 1.0.0 in {notion_root}.\n\
             Its manifest calls it Notion.\n"
        )
    );
    let state = &official_row(&fixture)[2];
    assert!(
        state.starts_with("built in; updated ")
            && state.ends_with(&format!(" at {}", &official_commit[..12])),
        "{state}"
    );
    let fetcher_root = cached_root(
        &fixture,
        "claude-plugins-official",
        "fetcher",
        &external_commit[..12],
    );
    assert_eq!(
        succeeded(&plugin(
            &fixture,
            &["install", "fetcher@claude-plugins-official"]
        )),
        format!(
            "Installed plugin fetcher@claude-plugins-official {} in {fetcher_root}.\n",
            &external_commit[..12]
        )
    );
    let nested_root = cached_root(
        &fixture,
        "claude-plugins-official",
        "nested",
        &monorepo_commit[..12],
    );
    assert_eq!(
        succeeded(&plugin(&fixture, &["install", "nested"])),
        format!(
            "Installed plugin nested@claude-plugins-official {} in {nested_root}.\n",
            &monorepo_commit[..12]
        )
    );
    for (root, skill) in [
        (&notion_root, "pages"),
        (&fetcher_root, "fetch"),
        (&nested_root, "nest"),
    ] {
        assert!(
            Path::new(root)
                .join("skills")
                .join(skill)
                .join("SKILL.md")
                .is_file()
        );
    }
    assert!(!Path::new(&nested_root).join("other").exists());
    assert_eq!(
        rows(&succeeded(&plugin(&fixture, &["list"]))),
        [
            vec![
                "fetcher@claude-plugins-official".to_owned(),
                external_commit[..12].to_owned(),
                "enabled".to_owned(),
                "no login needed".to_owned(),
            ],
            vec![
                "nested@claude-plugins-official".to_owned(),
                monorepo_commit[..12].to_owned(),
                "enabled".to_owned(),
                "no login needed".to_owned(),
            ],
            vec![
                "notion@claude-plugins-official".to_owned(),
                "1.0.0".to_owned(),
                "enabled".to_owned(),
                "no login needed".to_owned(),
                "manifest name: Notion".to_owned(),
            ],
        ]
    );
}

#[test]
fn a_tick_package_installs_and_updates_by_name_from_the_tickloop_marketplace() {
    let fixture = Fixture::new();
    official_marketplace(&fixture, json!([]));
    let server = Server::start();
    let offer = |version: &str| {
        let (archive, sha256) = archive_of(&fixture.package_named("sample", version));
        let url = server.serve(
            &format!("/tick-sample-{version}.tar.gz"),
            Reply::Body(archive),
        );
        fixture.marketplace_file(
            DEFAULT_TICK_MARKETPLACE,
            &marketplace_json(json!([offered(
                "sample",
                version,
                &[(CURRENT_TARGET, &url, &sha256)]
            )])),
        );
    };
    offer("1.0.0");
    assert_eq!(
        failed(&plugin(&fixture, &["install", "sample"])),
        "Error: sample carries a Tick program; use tickloop tick install sample\n"
    );
    assert_eq!(
        succeeded(&tick(&fixture, &["install", "sample"])),
        format!(
            "Installed Tick sample@tickloop 1.0.0 in {}.\n\
             When opened, its Tick program runs as your user. Installing runs nothing.\n",
            cached_root(&fixture, "tickloop", "sample", "1.0.0")
        )
    );
    assert_eq!(
        succeeded(&tick(&fixture, &["update", "sample"])),
        "Tick sample@tickloop 1.0.0 is up to date.\n"
    );
    offer("2.0.0");
    assert_eq!(
        succeeded(&plugin(&fixture, &["update"])),
        "No plugins are installed.\n"
    );
    assert_eq!(
        succeeded(&tick(&fixture, &["update"])),
        format!(
            "Updated Tick sample@tickloop from 1.0.0 to 2.0.0 in {}.\n\
             When opened, its Tick program runs as your user. Installing runs nothing.\n",
            cached_root(&fixture, "tickloop", "sample", "2.0.0")
        )
    );
}

#[test]
fn the_default_claude_marketplace_can_be_removed_and_added_back_by_source() {
    let fixture = Fixture::new();
    let official = fixture.root.join(OFFICIAL_MARKETPLACE_REPOSITORY);
    write_skill(&official.join("review/skills/check"), "check", "Review");
    official_marketplace(&fixture, json!([{"name": "review", "source": "./review"}]));
    succeeded(&plugin(
        &fixture,
        &["install", "review@claude-plugins-official"],
    ));
    for source in [
        "anthropics/claude-plugins-official",
        "https://github.com/anthropics/claude-plugins-official.git",
    ] {
        let removed = succeeded(&plugin(
            &fixture,
            &["marketplace", "remove", "claude-plugins-official"],
        ));
        assert!(removed.contains("add it back with tickloop plugin marketplace add anthropics/claude-plugins-official"), "{removed}");
        assert!(
            removed.contains("Its plugins stay installed: review@claude-plugins-official"),
            "{removed}"
        );
        assert!(
            !succeeded(&plugin(&fixture, &["marketplace", "list"]))
                .contains("claude-plugins-official")
        );
        assert!(succeeded(&plugin(&fixture, &["list"])).contains("review@claude-plugins-official"));
        assert_eq!(
            failed(&plugin(&fixture, &["update", "review"])),
            "Error: cannot update review@claude-plugins-official: marketplace claude-plugins-official is not registered\n"
        );
        assert!(
            succeeded(&plugin(&fixture, &["update"]))
                .contains("is not updated: marketplace claude-plugins-official is not registered")
        );
        let impostor =
            marketplace_directory(&fixture, "impostor", "claude-plugins-official", json!([]));
        assert!(
            failed(&plugin(
                &fixture,
                &["marketplace", "add", &path_text(&impostor)]
            ))
            .contains("reserved")
        );
        succeeded(&plugin(&fixture, &["marketplace", "add", source]));
        let listed = succeeded(&plugin(&fixture, &["marketplace", "list"]));
        assert!(
            listed
                .lines()
                .any(|line| line.starts_with("claude-plugins-official")
                    && line.contains("built in; updated")),
            "{listed}"
        );
        assert!(succeeded(&plugin(&fixture, &["update", "review"])).contains("is up to date"));
    }
}
