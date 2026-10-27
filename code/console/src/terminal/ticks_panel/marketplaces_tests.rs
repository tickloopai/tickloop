//! The Marketplaces page of `/plugins`: what it lists and the work its keys ask for.

use std::path::PathBuf;

use tick_plugins::{GitSource, MarketplaceRegistration, MarketplaceSource};

use super::*;

const WIDTH: usize = 130;
const HEIGHT: usize = 16;

fn registration(name: &str, source: MarketplaceSource, builtin: bool) -> MarketplaceRegistration {
    MarketplaceRegistration {
        name: name.into(),
        source,
        builtin,
        refreshed_at: None,
        commit: None,
    }
}

/// The built-in marketplaces, `tickloop` read from a file and `claude-plugins-official` not
/// fetched yet, beside `team`, last updated at a commit, and `broken`, which cannot be read.
fn offers() -> Vec<MarketplaceOffer> {
    let team = MarketplaceRegistration {
        refreshed_at: Some("2026-10-27T08:00:00Z".into()),
        commit: Some("0123456789abcdef0123".into()),
        ..registration(
            "team",
            MarketplaceSource::Git(GitSource {
                url: "file:///sources/team".into(),
                reference: Some("main".into()),
                sha: None,
                subdirectory: None,
            }),
            false,
        )
    };
    vec![
        MarketplaceOffer {
            registration: registration(
                "tickloop",
                MarketplaceSource::File(PathBuf::from("/ticks/marketplace.json")),
                true,
            ),
            plugins: Ok(Vec::new()),
        },
        MarketplaceOffer {
            registration: registration(
                "claude-plugins-official",
                MarketplaceSource::Git(GitSource {
                    url: "https://github.com/anthropics/claude-plugins-official.git".into(),
                    reference: None,
                    sha: None,
                    subdirectory: None,
                }),
                true,
            ),
            plugins: Ok(Vec::new()),
        },
        MarketplaceOffer {
            registration: team,
            plugins: Ok(Vec::new()),
        },
        MarketplaceOffer {
            registration: registration(
                "broken",
                MarketplaceSource::Directory(PathBuf::from("/sources/broken")),
                false,
            ),
            plugins: Err("cannot read /sources/broken/marketplace.json".into()),
        },
    ]
}

fn panel(kind: PluginKind) -> TicksPanel {
    let mut panel = TicksPanel::new(None, kind);
    panel.apply(TickPackageResult::Installed {
        read: 1,
        listing: Ok(InstalledList::default()),
    });
    panel.apply(TickPackageResult::Marketplace {
        read: 1,
        listing: Ok(offers()),
    });
    panel
}

fn key(panel: &mut TicksPanel, code: KeyCode) -> TicksPanelEffect {
    panel.key(KeyEvent::new(code, KeyModifiers::NONE))
}

fn text(panel: &mut TicksPanel) -> String {
    panel
        .frame(WIDTH, HEIGHT, DisplayProfile::default())
        .lines
        .iter()
        .map(RoleLine::text)
        .collect::<Vec<_>>()
        .join("\n")
}

/// The line of the row named `name`.
fn row(panel: &mut TicksPanel, name: &str) -> String {
    let shown = text(panel);
    shown
        .lines()
        .find(|line| line.starts_with(&format!("◉ {name} ")))
        .unwrap_or_else(|| panic!("no row {name}:\n{shown}"))
        .to_owned()
}

#[test]
fn the_plugins_panel_lists_every_marketplace_as_the_command_line_does_on_a_third_page() {
    let mut panel = panel(PluginKind::Plugin);
    key(&mut panel, KeyCode::Tab);
    key(&mut panel, KeyCode::Tab);
    let shown = text(&mut panel);
    assert!(
        shown.starts_with("Plugins · Installed  Available  [Marketplaces]"),
        "{shown}"
    );
    assert!(
        row(&mut panel, "tickloop")
            .ends_with("/ticks/marketplace.json · built in; read on every use"),
        "{shown}"
    );
    assert!(
        row(&mut panel, "claude-plugins-official").ends_with(
            "https://github.com/anthropics/claude-plugins-official.git · built in; not fetched yet"
        ),
        "{shown}"
    );
    assert!(
        row(&mut panel, "team")
            .ends_with("file:///sources/team#main · updated 2026-10-27T08:00:00Z at 0123456789ab"),
        "{shown}"
    );
    assert!(
        shown.contains(
            "cannot read marketplace broken: cannot read /sources/broken/marketplace.json"
        ),
        "{shown}"
    );
    assert!(
        shown.contains("enter update")
            && shown.contains("a add")
            && shown.contains("x remove")
            && shown.contains("esc close"),
        "{shown}"
    );
    key(&mut panel, KeyCode::Tab);
    assert!(text(&mut panel).starts_with("Plugins · [Installed]"));
    key(&mut panel, KeyCode::BackTab);
    assert!(text(&mut panel).starts_with("Plugins · Installed  Available  [Marketplaces]"));
}

#[test]
fn the_ticks_panel_has_no_marketplaces_page_and_points_to_plugins() {
    let mut panel = panel(PluginKind::Tick);
    key(&mut panel, KeyCode::Tab);
    let shown = text(&mut panel);
    assert!(
        shown.starts_with("Ticks · Installed  [Available]"),
        "{shown}"
    );
    assert!(!shown.contains("Marketplaces"), "{shown}");
    assert!(shown.contains("Manage marketplaces in /plugins"), "{shown}");
    key(&mut panel, KeyCode::Tab);
    assert!(text(&mut panel).starts_with("Ticks · [Installed]  Available"));
}

fn marketplaces_page() -> TicksPanel {
    let mut panel = panel(PluginKind::Plugin);
    key(&mut panel, KeyCode::BackTab);
    panel
}

fn work(effect: TicksPanelEffect) -> TickPackageRequest {
    match effect {
        TicksPanelEffect::Work(request) => request,
        TicksPanelEffect::None => panic!("no work started"),
        TicksPanelEffect::Close => panic!("the panel closed"),
        TicksPanelEffect::EditFilter => panic!("the external editor opened"),
    }
}

fn type_text(panel: &mut TicksPanel, text: &str) {
    for character in text.chars() {
        assert!(matches!(
            key(panel, KeyCode::Char(character)),
            TicksPanelEffect::None
        ));
    }
}

#[test]
fn a_marketplace_is_added_from_a_single_line_field_that_keeps_list_keys_as_text() {
    let mut panel = marketplaces_page();
    key(&mut panel, KeyCode::Char('a'));
    let shown = text(&mut panel);
    assert!(shown.contains("Add marketplace: ▏"), "{shown}");
    assert!(shown.contains(MARKETPLACE_SOURCES), "{shown}");
    assert!(
        shown.ends_with("enter add · ctrl+g editor · esc cancel"),
        "{shown}"
    );
    type_text(&mut panel, "jkx/a");
    assert_eq!(panel.filter_text(), "jkx/a");
    assert!(matches!(
        panel.key(KeyEvent::new(KeyCode::Char('g'), KeyModifiers::CONTROL)),
        TicksPanelEffect::EditFilter
    ));
    panel.replace_filter("  /sources/team\n");
    assert!(text(&mut panel).contains("Add marketplace: /sources/team"));
    assert_eq!(
        work(key(&mut panel, KeyCode::Enter)),
        TickPackageRequest::AddMarketplace("/sources/team".into())
    );
    let shown = text(&mut panel);
    assert!(
        shown.contains("Adding marketplace /sources/team…") && !shown.contains("Add marketplace:"),
        "{shown}"
    );
    panel.apply(TickPackageResult::MarketplaceAdded {
        source: "/sources/team".into(),
        result: Err("\"/sources/team\" is not a marketplace path, URL or GitHub repository".into()),
    });
    assert!(text(&mut panel).contains(
        "Cannot add marketplace /sources/team: \"/sources/team\" is not a marketplace path"
    ));
}

#[test]
fn escape_cancels_adding_a_marketplace_and_an_empty_source_adds_nothing() {
    let mut panel = marketplaces_page();
    key(&mut panel, KeyCode::Char('a'));
    assert!(matches!(
        key(&mut panel, KeyCode::Enter),
        TicksPanelEffect::None
    ));
    type_text(&mut panel, "owner/repo");
    assert!(matches!(
        key(&mut panel, KeyCode::Esc),
        TicksPanelEffect::None
    ));
    let shown = text(&mut panel);
    assert!(!shown.contains("Add marketplace:"), "{shown}");
    assert!(shown.contains("enter update · a add"), "{shown}");
    assert!(matches!(
        key(&mut panel, KeyCode::Esc),
        TicksPanelEffect::Close
    ));
}

#[test]
fn enter_updates_the_selected_marketplace_and_reports_what_it_offers() {
    let mut panel = marketplaces_page();
    key(&mut panel, KeyCode::Char('j'));
    key(&mut panel, KeyCode::Char('j'));
    assert_eq!(
        work(key(&mut panel, KeyCode::Enter)),
        TickPackageRequest::UpdateMarketplace("team".into())
    );
    assert!(text(&mut panel).contains("Updating marketplace team…"));
    panel.apply(TickPackageResult::MarketplaceUpdated {
        name: "team".into(),
        result: Ok(3),
    });
    assert!(text(&mut panel).contains("Updated marketplace team; it offers 3 plugins"));
    key(&mut panel, KeyCode::Enter);
    panel.apply(TickPackageResult::MarketplaceUpdated {
        name: "team".into(),
        result: Err("git fetch failed".into()),
    });
    assert!(text(&mut panel).contains("Cannot update marketplace team: git fetch failed"));
}

#[test]
fn removing_a_marketplace_takes_two_x_presses_and_keeps_its_plugins() {
    let mut panel = marketplaces_page();
    for _ in 0..2 {
        key(&mut panel, KeyCode::Char('j'));
    }
    key(&mut panel, KeyCode::Char('x'));
    let shown = text(&mut panel);
    assert!(
        shown.ends_with("Press x again to remove marketplace team"),
        "{shown}"
    );
    assert!(
        shown.contains("Removing a marketplace keeps the plugins installed from it."),
        "{shown}"
    );
    key(&mut panel, KeyCode::Char('j'));
    assert!(!text(&mut panel).contains("Press x again"));
    key(&mut panel, KeyCode::Char('k'));
    key(&mut panel, KeyCode::Char('x'));
    assert_eq!(
        work(key(&mut panel, KeyCode::Char('x'))),
        TickPackageRequest::RemoveMarketplace("team".into())
    );
    assert!(text(&mut panel).contains("Removing marketplace team…"));
    panel.apply(TickPackageResult::MarketplaceRemoved {
        name: "team".into(),
        result: Ok(vec!["notes@team".into()]),
    });
    assert!(
        text(&mut panel)
            .contains("Removed marketplace team; its plugins stay installed: notes@team")
    );
}

#[test]
fn a_built_in_marketplace_says_why_it_cannot_be_removed() {
    let mut panel = marketplaces_page();
    for name in ["tickloop", "claude-plugins-official"] {
        for _ in 0..2 {
            assert!(matches!(
                key(&mut panel, KeyCode::Char('x')),
                TicksPanelEffect::None
            ));
        }
        let shown = text(&mut panel);
        assert!(
            shown.contains(&format!(
                "The built-in marketplace {name} cannot be removed"
            )) && !shown.contains("Press x again"),
            "{shown}"
        );
        key(&mut panel, KeyCode::Char('j'));
    }
}
