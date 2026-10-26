use tick_plugins::{CLAUDE_OFFICIAL_MARKETPLACE, GitSource, MarketplaceSource};

use crate::support::Fixture;

#[test]
fn marketplace_sources_expand_github_and_builtins_are_lazy() {
    assert_eq!(
        MarketplaceSource::parse("owner/repo#release").unwrap(),
        MarketplaceSource::Git(GitSource {
            url: "https://github.com/owner/repo.git".into(),
            reference: Some("release".into()),
            sha: None,
            subdirectory: None,
        })
    );
    let fixture = Fixture::new();
    let marketplaces = fixture.store.marketplaces().unwrap();
    assert_eq!(marketplaces.len(), 2);
    assert!(
        marketplaces
            .iter()
            .all(|marketplace| marketplace.builtin && marketplace.refreshed_at.is_none())
    );
    assert!(
        marketplaces
            .iter()
            .any(|marketplace| marketplace.name == CLAUDE_OFFICIAL_MARKETPLACE)
    );
    assert!(!fixture.store.directory().exists());
    for name in ["tickloop", CLAUDE_OFFICIAL_MARKETPLACE] {
        assert!(
            fixture
                .store
                .remove_marketplace(name)
                .unwrap_err()
                .to_string()
                .contains("built-in")
        );
    }
}

use crate::support::write_json;
use serde_json::json;

fn no_http(_: &reqwest::Url, _: tick_plugins::HttpRequest) -> Result<reqwest::Client, String> {
    panic!("tests must not access the network")
}

#[tokio::test]
async fn a_local_marketplace_is_registered_refreshed_and_removed() {
    let fixture = Fixture::new();
    let root = fixture.source("market");
    let path = root.join(".claude-plugin/marketplace.json");
    write_json(
        &path,
        &json!({"name": "sample", "plugins": [
            {"name": "review", "source": "./plugins/review", "strict": false},
            {"name": "unsupported", "source": {"source": "npm", "package": "example"}}
        ]}),
    );
    let added = fixture
        .store
        .add_marketplace(&MarketplaceSource::Directory(root.clone()), &no_http)
        .await
        .unwrap();
    assert_eq!(added.registration.name, "sample");
    assert_eq!(added.plugins.len(), 2);
    assert!(added.plugins[0].is_installable());
    assert!(!added.plugins[1].is_installable());
    assert!(added.registration.refreshed_at.is_some());
    assert_eq!(fixture.store.marketplaces().unwrap().len(), 3);
    write_json(&path, &json!({"name": "sample", "plugins": []}));
    assert_eq!(
        fixture
            .store
            .marketplace("sample", &no_http)
            .await
            .unwrap()
            .plugins
            .len(),
        2
    );
    assert!(
        fixture
            .store
            .refresh_marketplace("sample", &no_http)
            .await
            .unwrap()
            .plugins
            .is_empty()
    );
    fixture.store.remove_marketplace("sample").unwrap();
    assert_eq!(fixture.store.marketplaces().unwrap().len(), 2);
}

#[tokio::test]
async fn marketplace_lookup_prefers_tickloop_then_claude_then_root_and_reads_tick_packages() {
    let fixture = Fixture::new();
    let root = fixture.source("formats");
    write_json(
        &root.join("marketplace.json"),
        &json!({"name": "root-market", "owner": {"name": "Test"}, "packages": [{
            "name": "quotes", "version": "1.0", "description": "Quotes", "downloads": {
                (tick_packages::CURRENT_TARGET): {"url": "https://example.test/quotes.tar.gz", "sha256": "a".repeat(64)}
            }
        }]}),
    );
    write_json(
        &root.join(".claude-plugin/marketplace.json"),
        &json!({"name": "claude-market", "plugins": []}),
    );
    write_json(
        &root.join(".tickloop-plugin/marketplace.json"),
        &json!({"name": "tickloop-market", "plugins": []}),
    );
    let source = MarketplaceSource::Directory(root.clone());
    assert_eq!(
        fixture
            .store
            .add_marketplace(&source, &no_http)
            .await
            .unwrap()
            .registration
            .name,
        "tickloop-market"
    );
    std::fs::remove_file(root.join(".tickloop-plugin/marketplace.json")).unwrap();
    assert_eq!(
        fixture
            .store
            .add_marketplace(&source, &no_http)
            .await
            .unwrap()
            .registration
            .name,
        "claude-market"
    );
    std::fs::remove_file(root.join(".claude-plugin/marketplace.json")).unwrap();
    let catalog = fixture
        .store
        .add_marketplace(&source, &no_http)
        .await
        .unwrap();
    assert_eq!(catalog.registration.name, "root-market");
    assert_eq!(catalog.plugins[0].version.as_deref(), Some("1.0"));
    assert!(catalog.plugins[0].is_installable());
    assert!(
        matches!(&catalog.plugins[0].source, tick_plugins::MarketplacePluginSource::Archive(package) if package.current_download().unwrap().sha256 == "a".repeat(64))
    );
}

fn git(root: &std::path::Path, args: &[&str]) -> String {
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
        ])
        .args(args)
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

#[tokio::test]
async fn git_marketplace_installs_relative_plugins_by_commit_and_keeps_same_revision() {
    let fixture = Fixture::new();
    let root = fixture.source("git-market");
    write_json(
        &root.join(".claude-plugin/marketplace.json"),
        &json!({"name": "git-market", "plugins": [
            {"name": "review", "source": "./plugins/review", "strict": false, "skills": "./custom"}
        ]}),
    );
    crate::support::write_skill(&root.join("plugins/review/custom"), "check");
    git(&root, &["init", "-b", "main"]);
    git(&root, &["add", "."]);
    git(&root, &["commit", "-m", "initial"]);
    let commit = git(&root, &["rev-parse", "HEAD"]);
    let source = MarketplaceSource::parse(&format!("file://{}", root.display())).unwrap();
    let catalog = fixture
        .store
        .add_marketplace(&source, &no_http)
        .await
        .unwrap();
    assert_eq!(
        catalog.registration.commit.as_deref(),
        Some(commit.as_str())
    );
    let installed = fixture
        .store
        .install_marketplace_plugin("git-market", "review", &no_http)
        .await
        .unwrap();
    assert_eq!(installed.plugin.revision, commit[..12]);
    assert_eq!(installed.plugin.version, commit[..12]);
    assert_eq!(installed.plugin.id(), "review@git-market");
    assert_eq!(
        installed.plugin.skill_directories(),
        vec![installed.plugin.root.join("custom")]
    );
    assert!(!installed.plugin.root.join(".git").exists());
    assert!(
        matches!(installed.plugin.source, tick_plugins::PluginSource::Git { ref commit, .. } if commit.len() == 40)
    );
    fixture.store.set_enabled("review", false).unwrap();
    let before = fixture.registry();
    let again = fixture
        .store
        .install_marketplace_plugin("git-market", "review", &no_http)
        .await
        .unwrap();
    assert_eq!(
        again.outcome,
        tick_plugins::InstallOutcome::AlreadyInstalled
    );
    assert!(!again.plugin.enabled);
    assert_eq!(before, fixture.registry());
    assert!(
        fixture
            .store
            .get("review")
            .unwrap()
            .root
            .join(".claude-plugin/plugin.json")
            .exists()
    );
}

#[tokio::test]
async fn git_refs_sha_pins_and_sparse_subdirectories_install_the_selected_content() {
    let fixture = Fixture::new();
    let root = fixture.source("repo");
    crate::support::write_skill(&root.join("plugins/review/skills/old"), "old");
    std::fs::write(root.join("outside"), "not part of plugin").unwrap();
    git(&root, &["init", "-b", "main"]);
    git(&root, &["add", "."]);
    git(&root, &["commit", "-m", "old"]);
    let old = git(&root, &["rev-parse", "HEAD"]);
    git(&root, &["tag", "v1"]);
    std::fs::remove_dir_all(root.join("plugins/review/skills/old")).unwrap();
    crate::support::write_skill(&root.join("plugins/review/skills/new"), "new");
    git(&root, &["add", "."]);
    git(&root, &["commit", "-m", "new"]);
    let newest = git(&root, &["rev-parse", "HEAD"]);
    let market = fixture.source("sources-market");
    write_json(
        &market.join("marketplace.json"),
        &json!({"name": "sources", "plugins": [
            {"name": "review", "source": {"source": "git-subdir", "url": format!("file://{}", root.display()), "path": "plugins/review", "ref": "v1", "sha": old}},
            {"name": "whole", "source": {"source": "url", "url": format!("file://{}", root.display()), "sha": newest}},
            {"name": "github", "source": {"source": "github", "repo": "owner/repo", "ref": "main", "sha": old}}
        ]}),
    );
    let catalog = fixture
        .store
        .add_marketplace(&MarketplaceSource::Directory(market.clone()), &no_http)
        .await
        .unwrap();
    assert!(catalog.plugins.iter().all(|plugin| plugin.is_installable()));
    let installed = fixture
        .store
        .install_marketplace_plugin("sources", "review", &no_http)
        .await
        .unwrap()
        .plugin;
    assert_eq!(installed.revision, old[..12]);
    assert!(installed.root.join("skills/old/SKILL.md").exists());
    assert!(!installed.root.join("skills/new").exists());
    assert!(!installed.root.join("outside").exists());
    let whole = fixture
        .store
        .install_marketplace_plugin("sources", "whole", &no_http)
        .await
        .unwrap()
        .plugin;
    assert_eq!(whole.revision, newest[..12]);
    assert!(whole.root.join("outside").is_file());
    let mut document: serde_json::Value =
        serde_json::from_slice(&std::fs::read(market.join("marketplace.json")).unwrap()).unwrap();
    document["plugins"][0]["source"]["sha"] = json!(newest);
    write_json(&market.join("marketplace.json"), &document);
    fixture
        .store
        .refresh_marketplace("sources", &no_http)
        .await
        .unwrap();
    let before = fixture.registry();
    let error = fixture
        .store
        .install_marketplace_plugin("sources", "review", &no_http)
        .await
        .unwrap_err();
    assert!(error.to_string().contains("SHA"), "{error}");
    assert_eq!(before, fixture.registry());
    assert!(installed.root.join("skills/old/SKILL.md").exists());
}
