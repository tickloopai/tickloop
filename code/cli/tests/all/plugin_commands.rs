//! `tickloop plugin` as a user runs it, against a private config root, with plugins from local
//! directories.

use std::{
    fs,
    path::{Path, PathBuf},
};

use serde_json::{Value, json};

use crate::tick_package_commands::{Fixture, failed, path_text, succeeded};

/// A Claude Code plugin directory `label` with `manifest`, a Skill per name in `skills/`, and the
/// commands Tickloop does not use yet.
fn claude_plugin(fixture: &Fixture, label: &str, manifest: Value, skills: &[&str]) -> PathBuf {
    let directory = fixture.root.join("sources").join(label);
    fs::create_dir_all(directory.join(".claude-plugin")).unwrap();
    fs::write(
        directory.join(".claude-plugin/plugin.json"),
        manifest.to_string(),
    )
    .unwrap();
    for skill in skills {
        let skill_directory = directory.join("skills").join(skill);
        fs::create_dir_all(&skill_directory).unwrap();
        fs::write(
            skill_directory.join("SKILL.md"),
            format!("---\nname: {skill}\ndescription: Uses {skill}\n---\nBody\n"),
        )
        .unwrap();
    }
    fs::create_dir_all(directory.join("commands")).unwrap();
    fs::write(directory.join("commands/tidy.md"), "Tidy up\n").unwrap();
    fs::write(directory.join(".mcp.json"), "{}").unwrap();
    directory
}

fn installed_root(fixture: &Fixture, name: &str) -> String {
    path_text(
        &fixture
            .root
            .join("config/plugins/cache/local")
            .join(name)
            .join("local"),
    )
}

fn plugin(fixture: &Fixture, arguments: &[&str]) -> std::process::Output {
    let arguments = [&["plugin"][..], arguments].concat();
    fixture.tickloop(&arguments)
}

#[test]
fn a_claude_plugin_installs_lists_toggles_and_uninstalls_through_the_cli() {
    let fixture = Fixture::new();
    assert_eq!(
        succeeded(&plugin(&fixture, &["list"])),
        "No plugins are installed.\n"
    );
    let source = claude_plugin(
        &fixture,
        "notes",
        json!({"name": "notes", "version": "1.0.0", "skills": ["./skills", "./missing"]}),
        &["write"],
    );
    let source_path = path_text(&source);
    let root = installed_root(&fixture, "notes");

    assert_eq!(
        succeeded(&plugin(&fixture, &["install", "--path", &source_path])),
        format!(
            "Installed plugin notes@local 1.0.0 in {root}.\n\
             Not supported yet, so not used: commands.\n\
             Warning: skills path \"./missing\" is ignored: it does not exist\n"
        )
    );
    assert_eq!(
        succeeded(&plugin(&fixture, &["list"])),
        "notes@local  1.0.0  enabled  no login needed  not supported yet: commands\n"
    );

    assert_eq!(
        succeeded(&plugin(&fixture, &["disable", "notes"])),
        "Disabled plugin notes@local; its Skills stop loading from the next request.\n"
    );
    assert_eq!(
        succeeded(&plugin(&fixture, &["disable", "notes"])),
        "Plugin notes@local is already disabled.\n"
    );
    let reinstalled = succeeded(&plugin(&fixture, &["install", "--path", &source_path]));
    assert!(
        reinstalled.starts_with(&format!(
            "Reinstalled plugin notes@local 1.0.0 in {root}.\n\
             It stays disabled; tickloop plugin enable notes turns it on.\n"
        )),
        "{reinstalled}"
    );
    assert_eq!(
        succeeded(&plugin(&fixture, &["list"])),
        "notes@local  1.0.0  disabled  no login needed  not supported yet: commands\n"
    );
    assert_eq!(
        succeeded(&plugin(&fixture, &["enable", "notes"])),
        "Enabled plugin notes@local; its Skills load from the next request.\n"
    );

    fs::write(
        source.join(".claude-plugin/plugin.json"),
        json!({"name": "notes", "version": "1.1.0"}).to_string(),
    )
    .unwrap();
    fs::remove_dir_all(source.join("commands")).unwrap();
    fs::remove_file(source.join(".mcp.json")).unwrap();
    assert_eq!(
        succeeded(&plugin(&fixture, &["install", "--path", &source_path])),
        format!("Replaced plugin notes@local 1.0.0 with 1.1.0 in {root}.\n")
    );
    assert_eq!(
        succeeded(&plugin(&fixture, &["list"])),
        "notes@local  1.1.0  enabled  no login needed\n"
    );

    let data = fixture.root.join("config/plugins/data/notes");
    fs::create_dir_all(&data).unwrap();
    assert_eq!(
        succeeded(&plugin(&fixture, &["uninstall", "notes", "--keep-data"])),
        format!(
            "Uninstalled plugin notes@local 1.1.0.\nIts data stays in {}.\n",
            path_text(&data)
        )
    );
    assert!(data.is_dir());
    assert!(!Path::new(&root).exists());
    assert_eq!(
        failed(&plugin(&fixture, &["uninstall", "notes"])),
        "Error: plugin notes is not installed\n"
    );
    assert_eq!(
        failed(&plugin(&fixture, &["enable", "notes"])),
        "Error: plugin notes is not installed\n"
    );

    succeeded(&plugin(&fixture, &["install", "--path", &source_path]));
    assert_eq!(
        succeeded(&plugin(&fixture, &["uninstall", "notes"])),
        "Uninstalled plugin notes@local 1.1.0.\n"
    );
    assert!(!data.exists());
    assert_eq!(
        succeeded(&plugin(&fixture, &["list"])),
        "No plugins are installed.\n"
    );
}

#[test]
fn validate_shows_what_tickloop_reads_from_a_plugin_and_why_one_cannot_be_used() {
    let fixture = Fixture::new();
    let source = claude_plugin(
        &fixture,
        "notes",
        json!({"name": "notes", "version": "1.0.0", "skills": ["./skills", "../outside"]}),
        &["read", "write"],
    );

    assert_eq!(
        succeeded(&plugin(&fixture, &["validate", &path_text(&source)])),
        "Plugin notes 1.0.0, read from .claude-plugin/plugin.json.\n\
         Skills: skills/read, skills/write\n\
         Not supported yet, so not used: commands.\n\
         Warning: skills path \"../outside\" is ignored: it must start with ./\n"
    );

    let bare = fixture.root.join("sources/bare-plugin");
    fs::create_dir_all(&bare).unwrap();
    assert_eq!(
        succeeded(&plugin(&fixture, &["validate", &path_text(&bare)])),
        "Plugin bare-plugin, read from its directory.\nSkills: none\n"
    );

    fs::write(source.join(".claude-plugin/plugin.json"), r#"{"name": 7}"#).unwrap();
    assert_eq!(
        failed(&plugin(&fixture, &["validate", &path_text(&source)])),
        format!(
            "Error: {} is not a valid plugin: name in .claude-plugin/plugin.json must be a string\n",
            source.display()
        )
    );
    let missing = fixture.root.join("missing");
    assert_eq!(
        failed(&plugin(&fixture, &["validate", &path_text(&missing)])),
        format!("Error: {} is not a plugin directory\n", missing.display())
    );
    assert!(
        !fixture.root.join("config").exists(),
        "validate installs nothing"
    );
}

#[test]
fn a_local_tick_plugin_can_be_replaced_and_an_unusable_plugin_is_listed() {
    let fixture = Fixture::new();
    succeeded(&fixture.tickloop(&[
        "tick",
        "install",
        "--path",
        &path_text(&fixture.package_named("sample", "1.0.0")),
    ]));
    let clash = claude_plugin(&fixture, "sample", json!({"name": "sample"}), &[]);

    let replaced = succeeded(&plugin(
        &fixture,
        &["install", "--path", &path_text(&clash)],
    ));
    assert!(
        replaced.contains("Replaced plugin sample@local"),
        "{replaced}"
    );
    assert!(succeeded(&plugin(&fixture, &["list"])).contains("sample@local"));
    succeeded(&plugin(&fixture, &["uninstall", "sample"]));

    let notes = claude_plugin(&fixture, "notes", json!({"name": "notes"}), &["write"]);
    succeeded(&plugin(
        &fixture,
        &["install", "--path", &path_text(&notes)],
    ));
    fs::remove_dir_all(installed_root(&fixture, "notes")).unwrap();
    let listed = succeeded(&plugin(&fixture, &["list"]));
    assert!(
        listed.starts_with(
            "notes@local  local  enabled  login unknown  unusable: its directory cannot be opened"
        ),
        "{listed}"
    );
    assert!(listed.ends_with("; install it again\n"), "{listed}");
    let error = failed(&plugin(&fixture, &["disable", "notes"]));
    assert!(
        error.starts_with("Error: cannot disable notes: plugin notes cannot be used"),
        "{error}"
    );
}

#[test]
fn the_plugin_command_explains_its_usage() {
    let fixture = Fixture::new();
    for arguments in [
        &["plugin"][..],
        &["plugin", "install"],
        &["plugin", "install", "--path"],
        &["plugin", "uninstall"],
        &["plugin", "uninstall", "notes", "--all"],
        &["plugin", "enable"],
        &["plugin", "disable", "--all"],
        &["plugin", "validate"],
        &["plugin", "update", "--all"],
        &["plugin", "marketplace"],
        &["plugin", "marketplace", "add"],
        &["plugin", "marketplace", "remove"],
        &["plugin", "marketplace", "list", "extra"],
        &["plugin", "login", "notes"],
        &["plugin", "logout", "notes"],
    ] {
        let error = failed(&fixture.tickloop(arguments));
        assert!(
            error.starts_with(
                "Error: Unknown plugin command. Usage: tickloop plugin install PLUGIN[@MARKETPLACE]"
            ),
            "{arguments:?}: {error}"
        );
    }
    for arguments in [
        &["plugin", "--help"][..],
        &["plugin", "-h"],
        &["plugin", "install", "--help"],
        &["plugin", "validate", "-h"],
        &["plugin", "update", "--help"],
        &["plugin", "marketplace", "--help"],
        &["plugin", "marketplace", "add", "--help"],
    ] {
        let help = succeeded(&fixture.tickloop(arguments));
        assert!(
            help.starts_with(
                "Usage: tickloop plugin install PLUGIN[@MARKETPLACE]\n       tickloop plugin install --path PATH\n"
            ),
            "{help}"
        );
        assert!(
            help.contains("       tickloop plugin marketplace add SOURCE\n"),
            "{help}"
        );
        assert!(help.contains("claude-plugins-official"), "{help}");
        assert!(help.contains("TICKLOOP_CONFIG_DIR"), "{help}");
        assert!(help.contains("named PLUGIN:SKILL"), "{help}");
        assert!(help.contains("grant no tools"), "{help}");
    }
    let console_help = succeeded(&fixture.tickloop(&["--help"]));
    assert!(
        console_help.contains(
            "tickloop plugin install PLUGIN[@MARKETPLACE] | install --path PATH | update [PLUGIN] | uninstall PLUGIN [--keep-data] | enable PLUGIN | disable PLUGIN | list [--available] | validate DIRECTORY | marketplace add SOURCE | marketplace list | marketplace update [MARKETPLACE] | marketplace remove MARKETPLACE"
        ),
        "{console_help}"
    );
    assert!(!fixture.root.join("config").exists());
}

#[test]
fn tick_and_plugin_each_manage_only_their_own_kind() {
    let fixture = Fixture::new();
    let tick = path_text(&fixture.package_named("sample", "1.0.0"));
    let notes = path_text(&claude_plugin(
        &fixture,
        "notes",
        json!({"name": "notes", "version": "1.0.0"}),
        &["write"],
    ));
    assert_eq!(
        failed(&plugin(&fixture, &["install", "--path", &tick])),
        format!("Error: sample carries a Tick program; use tickloop tick install --path {tick}\n")
    );
    assert_eq!(
        failed(&fixture.tickloop(&["tick", "install", "--path", &notes])),
        format!("Error: notes has no Tick program; use tickloop plugin install --path {notes}\n")
    );
    assert!(!fixture.root.join("config/plugins/cache/local").exists());

    let output = fixture.tickloop(&["tick", "install", "--path", &tick]);
    let installed = succeeded(&output);
    assert!(
        installed.starts_with("Installed Tick sample@local 1.0.0")
            && installed.contains("When opened, its Tick program runs as your user."),
        "{installed}"
    );
    assert!(output.stderr.is_empty(), "no rename notice");
    succeeded(&plugin(&fixture, &["install", "--path", &notes]));

    let ticks = succeeded(&fixture.tickloop(&["tick", "list"]));
    assert!(
        ticks.starts_with("sample@local  1.0.0  enabled  no login needed")
            && !ticks.contains("notes"),
        "{ticks}"
    );
    let plugins = succeeded(&plugin(&fixture, &["list"]));
    assert!(
        plugins.starts_with("notes@local  1.0.0  enabled") && !plugins.contains("sample"),
        "{plugins}"
    );

    for (arguments, error) in [
        (
            &["plugin", "disable", "sample"][..],
            "Error: sample carries a Tick program; use tickloop tick disable sample\n",
        ),
        (
            &["plugin", "uninstall", "sample"],
            "Error: sample carries a Tick program; use tickloop tick uninstall sample\n",
        ),
        (
            &["tick", "uninstall", "notes", "--keep-data"],
            "Error: notes has no Tick program; use tickloop plugin uninstall notes --keep-data\n",
        ),
        (
            &["tick", "update", "notes"],
            "Error: notes has no Tick program; use tickloop plugin update notes\n",
        ),
    ] {
        assert_eq!(failed(&fixture.tickloop(arguments)), error, "{arguments:?}");
    }
    assert_eq!(
        succeeded(&fixture.tickloop(&["tick", "disable", "sample"])),
        "Disabled Tick sample@local; its Skills stop loading from the next request.\n"
    );
    assert!(
        succeeded(&fixture.tickloop(&["tick", "uninstall", "sample"]))
            .starts_with("Uninstalled Tick sample@local 1.0.0.")
    );
    assert_eq!(
        succeeded(&fixture.tickloop(&["tick", "list"])),
        "No Ticks are installed.\n"
    );
    assert!(!fixture.root.join("config/ticks/installed.json").exists());
}

#[test]
fn first_cli_use_migrates_an_earlier_package_and_reports_it_only_once() {
    let fixture = Fixture::new();
    let source = fixture.package_with_auth(
        "sample",
        "1.0.0",
        json!({
            "type": "api_key", "env": "SAMPLE_API_KEY", "label": "Sample key"
        }),
    );
    let old = tick_packages::PackageStore::in_config_root(&fixture.root.join("config"));
    old.install_path(&source).unwrap();
    old.credentials("sample")
        .unwrap()
        .save_api_key(&tick_packages::ApiKey::new("saved-key").unwrap())
        .unwrap();
    let first = fixture.tickloop(&["tick", "list"]);
    assert!(first.status.success());
    assert!(
        String::from_utf8_lossy(&first.stderr)
            .contains("Migrated Tick packages to plugins: sample@local")
    );
    let listed = String::from_utf8_lossy(&first.stdout);
    assert!(
        listed.contains("sample@local  1.0.0  enabled  logged in"),
        "{listed}"
    );
    assert_eq!(succeeded(&fixture.tickloop(&["tick", "list"])), listed);
    assert!(!fixture.root.join("config/ticks").exists());
}

#[test]
fn validate_uses_marketplace_identity_only_when_explicitly_supplied() {
    let fixture = Fixture::new();
    let source = claude_plugin(&fixture, "notion", json!({"name": "Notion"}), &["read"]);
    let path = path_text(&source);
    let manifest_before = fs::read(source.join(".claude-plugin/plugin.json")).unwrap();
    assert!(failed(&plugin(&fixture, &["validate", &path])).contains("is not a plugin name"));
    let output = succeeded(&plugin(&fixture, &["validate", &path, "--name", "notion"]));
    assert!(
        output.contains("Plugin notion, read from .claude-plugin/plugin.json."),
        "{output}"
    );
    assert!(output.contains("Its manifest calls it Notion."), "{output}");
    assert!(output.contains("Skills: skills/read"), "{output}");
    assert!(
        failed(&plugin(&fixture, &["validate", &path, "--name", "Invalid"]))
            .contains("is not a plugin name")
    );
    assert_eq!(
        fs::read(source.join(".claude-plugin/plugin.json")).unwrap(),
        manifest_before
    );
    assert!(!fixture.root.join("config").exists());
}

#[test]
fn mcp_validation_install_and_enable_disclose_services_without_running_them() {
    let fixture = Fixture::new();
    let source = claude_plugin(
        &fixture,
        "mcp",
        json!({"name":"mcp", "mcpServers":{
            "local":{"command":"example-server", "args":["--serve"]},
            "remote":{"command":null,"url":"https://example.com/mcp"},
            "config":{"command":"example", "args":["${user_config.key}"]},
            "legacy":{"type":"sse", "url":"https://example.com/sse"}
        }}),
        &[],
    );
    let path = path_text(&source);
    for output in [
        succeeded(&plugin(&fixture, &["validate", &path])),
        succeeded(&plugin(&fixture, &["install", "--path", &path])),
    ] {
        assert!(
            output.contains("plugin_mcp_local")
                && output.contains("example-server")
                && output.contains("--serve"),
            "{output}"
        );
        assert!(
            output.contains("https://example.com/mcp")
                && output.contains("OAuth: discover on connect"),
            "{output}"
        );
        assert!(
            output.contains("userConfig: required") && output.contains("unsupported"),
            "{output}"
        );
        assert!(
            !output.contains("not used: commands, MCP servers"),
            "{output}"
        );
    }
    succeeded(&plugin(&fixture, &["disable", "mcp"]));
    let output = succeeded(&plugin(&fixture, &["enable", "mcp"]));
    assert!(
        output.contains("example-server") && output.contains("https://example.com/mcp"),
        "{output}"
    );
    assert!(output.contains("run as your user"), "{output}");
}

#[test]
fn mcp_install_and_enable_report_duplicates_against_all_enabled_sources() {
    let fixture = Fixture::new();
    fs::create_dir_all(fixture.root.join("config")).unwrap();
    fs::write(fixture.root.join("config/settings.json"), json!({
        "mcpServers":{"mine":{"url":"https://old.example/mcp"}},
        "projects":{path_text(&fixture.root):{"mcpServers":{"mine":{"url":"https://example.com/mcp"}}}}
    }).to_string()).unwrap();
    let alpha = claude_plugin(
        &fixture,
        "alpha",
        json!({"name":"alpha", "mcpServers":{"local":{"command":"shared-server"}}}),
        &[],
    );
    succeeded(&plugin(
        &fixture,
        &["install", "--path", &path_text(&alpha)],
    ));
    let zeta = claude_plugin(
        &fixture,
        "zeta",
        json!({"name":"zeta", "mcpServers":{
            "local":{"command":"shared-server"}, "remote":{"url":"https://example.com/mcp"}
        }}),
        &[],
    );
    let installed = succeeded(&plugin(&fixture, &["install", "--path", &path_text(&zeta)]));
    for text in [
        "plugin_zeta_local skipped: same command or URL as plugin_alpha_local",
        "plugin_zeta_remote skipped: same command or URL as mine",
    ] {
        assert!(installed.contains(text), "{installed}");
    }
    assert_eq!(
        installed.matches("status: skipped duplicate").count(),
        2,
        "{installed}"
    );
    succeeded(&plugin(&fixture, &["disable", "alpha"]));
    succeeded(&plugin(&fixture, &["disable", "zeta"]));
    let enabled = succeeded(&plugin(&fixture, &["enable", "zeta"]));
    assert_eq!(
        enabled.matches("status: skipped duplicate").count(),
        1,
        "{enabled}"
    );
    assert!(enabled.contains("plugin_zeta_local | transport: stdio | command: [\"shared-server\"] | OAuth: none | userConfig: none | status: ready"));
    let validated = succeeded(&plugin(&fixture, &["validate", &path_text(&zeta)]));
    assert!(!validated.contains("skipped duplicate"), "{validated}");
}

#[test]
fn user_config_install_without_terminal_succeeds_and_points_to_configure() {
    let fixture = Fixture::new();
    let source = claude_plugin(
        &fixture,
        "configured",
        json!({"name":"configured","userConfig":{"key":{"type":"string","required":true,"sensitive":true}},"mcpServers":{"remote":{"url":"https://example.com","headers":{"X-Key":"${user_config.key}"}}}}),
        &[],
    );
    let shown = succeeded(&plugin(
        &fixture,
        &["install", "--path", &path_text(&source)],
    ));
    assert!(
        shown.contains("tickloop plugin configure configured"),
        "{shown}"
    );
    let enabled = succeeded(&plugin(&fixture, &["enable", "configured"]));
    assert!(
        enabled.contains("tickloop plugin configure configured"),
        "{enabled}"
    );
    let configure = failed(&plugin(&fixture, &["configure", "configured"]));
    assert!(configure.contains("terminal"), "{configure}");
}

#[test]
fn user_config_terminal_prompts_only_for_missing_values_and_masks_reconfiguration() {
    let fixture = Fixture::new();
    let source = claude_plugin(
        &fixture,
        "configured",
        json!({"name":"configured","userConfig":{
            "key":{"type":"string","title":"Private key","required":true,"sensitive":true},
            "number":{"type":"number","default":4}
        }}),
        &[],
    );
    let mut terminal =
        fixture.tickloop_on_terminal(&["plugin", "install", "--path", &path_text(&source)]);
    terminal.wait_for("Private key (hidden as you type): ");
    terminal.type_text("cli-private-key\r");
    let (code, shown) = terminal.finish();
    assert_eq!(code, 0, "{shown}");
    assert!(!shown.contains("cli-private-key"));
    assert!(!shown.contains("number:"));
    let enabled = succeeded(&plugin(&fixture, &["enable", "configured"]));
    assert!(!enabled.contains("Configuration needed"));
    let mut terminal = fixture.tickloop_on_terminal(&["plugin", "configure", "configured"]);
    terminal.wait_for("Private key [Enter keeps ********] (hidden as you type): ");
    terminal.type_text("replaced-private-key\r");
    terminal.wait_for("number [Enter keeps 4]: ");
    terminal.type_text("7\r");
    let (code, shown) = terminal.finish();
    assert_eq!(code, 0, "{shown}");
    assert!(!shown.contains("private-key"));
    let public = fs::read_to_string(fixture.root.join("config/settings.json")).unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(&public).unwrap()["plugins"]["configured"]["options"]["number"],
        7
    );
}

#[test]
fn plugin_validate_uses_declared_user_config_defaults_without_saved_credentials() {
    let fixture = Fixture::new();
    let source = claude_plugin(
        &fixture,
        "configured",
        json!({
            "name":"configured","userConfig":{"host":{"type":"string","default":"example.com"}},
            "mcpServers":{"remote":{"url":"https://${user_config.host}/mcp"}}
        }),
        &[],
    );
    let shown = succeeded(&plugin(&fixture, &["validate", &path_text(&source)]));
    assert!(shown.contains("status: ready"), "{shown}");
}

#[test]
fn uninstalling_a_plugin_forgets_only_its_hook_trust_records() {
    let fixture = Fixture::new();
    let source = claude_plugin(&fixture, "guard", json!({"name": "guard"}), &[]);
    succeeded(&plugin(
        &fixture,
        &["install", "--path", &path_text(&source)],
    ));
    let settings_path = fixture.root.join("config/settings.json");
    let record = json!({"some-hook": {"trustedHash": "some-hash"}});
    fs::write(
        &settings_path,
        json!({"theme": "dark", "hookTrust": {"plugins": {
            "guard@local": record, "other@local": record,
        }}})
        .to_string(),
    )
    .unwrap();

    succeeded(&plugin(&fixture, &["uninstall", "guard"]));

    let settings: Value = serde_json::from_slice(&fs::read(&settings_path).unwrap()).unwrap();
    assert_eq!(
        settings,
        json!({"theme": "dark", "hookTrust": {"plugins": {"other@local": record}}})
    );
}
