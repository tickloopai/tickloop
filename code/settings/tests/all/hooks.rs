use std::{
    fs,
    path::{Path, PathBuf},
};

use serde_json::{Value, json};
use tick_plugins::{InstalledPlugin, PluginStore, SHIPPED_FILES_RECORD};
use tickloop_code_settings::UserSettings;
use tickloop_harness::policy::settings::{SettingsStore, SettingsStoreError};
use tickloop_harness::protocol::hooks::{
    ConfiguredHook, HookBackground, HookEvent, HookList, HookPluginDirectories, HookSource,
    HookTrust, HookTrustAction, HookTrustChange,
};

const DEFAULT_TIMEOUT_SECS: u64 = 600;

/// A temporary config root and Folder; nothing outside them is read.
struct Setup {
    directory: tempfile::TempDir,
    config_root: PathBuf,
    folder: PathBuf,
}

impl Setup {
    fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let config_root = directory.path().join("config");
        let folder = directory.path().join("folder");
        fs::create_dir_all(&config_root).unwrap();
        fs::create_dir_all(&folder).unwrap();
        Self {
            directory,
            config_root,
            folder,
        }
    }

    fn settings(&self) -> UserSettings {
        UserSettings::in_root(&self.config_root)
    }

    fn settings_path(&self) -> PathBuf {
        self.config_root.join("settings.json")
    }

    fn write_user_settings(&self, settings: Value) {
        fs::write(self.settings_path(), settings.to_string()).unwrap();
    }

    fn user_settings(&self) -> Value {
        serde_json::from_slice(&fs::read(self.settings_path()).unwrap()).unwrap()
    }

    fn write_repository_file(&self, folder: &Path, name: &str, contents: Value) -> PathBuf {
        let path = folder.join(".claude").join(name);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, contents.to_string()).unwrap();
        path
    }

    /// Installs the Claude Code plugin `name` with `manifest` and `files`, relative to its root.
    fn install_plugin(
        &self,
        name: &str,
        manifest: Value,
        files: &[(&str, &str)],
    ) -> InstalledPlugin {
        let source = self.directory.path().join("sources").join(name);
        let _ = fs::remove_dir_all(&source);
        fs::create_dir_all(source.join(".claude-plugin")).unwrap();
        fs::write(
            source.join(".claude-plugin/plugin.json"),
            manifest.to_string(),
        )
        .unwrap();
        for (path, contents) in files {
            let path = source.join(path);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, contents).unwrap();
        }
        self.plugins().install_path(&source).unwrap().plugin
    }

    fn plugins(&self) -> PluginStore {
        PluginStore::in_config_root(&self.config_root)
    }

    fn hooks(&self) -> HookList {
        self.settings().hooks(Some(&self.folder))
    }

    fn change(
        &self,
        hook: &ConfiguredHook,
        action: HookTrustAction,
    ) -> Result<(), SettingsStoreError> {
        let revision = self.hooks().revision;
        self.settings().write_hook_trust(
            Some(&self.folder),
            &[HookTrustChange {
                source: hook.source.clone(),
                key: hook.key.clone(),
                action,
            }],
            Some(&revision),
        )
    }

    fn trust(&self, hook: &ConfiguredHook) {
        let hash = hook.hash.clone().expect("only hashed hooks are trusted");
        self.change(hook, HookTrustAction::Trust { hash }).unwrap();
    }
}

fn hooks_of(event: &str, matcher: Option<&str>, commands: &[&str]) -> Value {
    let handlers = commands
        .iter()
        .map(|command| json!({"type": "command", "command": command}))
        .collect::<Vec<_>>();
    let mut group = json!({"hooks": handlers});
    if let Some(matcher) = matcher {
        group["matcher"] = json!(matcher);
    }
    json!({event: [group]})
}

fn hooks_json(hooks: Value) -> String {
    json!({"description": "test hooks", "hooks": hooks}).to_string()
}

fn only(list: &HookList, command: &str) -> ConfiguredHook {
    let found = list
        .hooks
        .iter()
        .filter(|hook| hook.command == command)
        .collect::<Vec<_>>();
    assert_eq!(found.len(), 1, "{command}: {:#?}", list.hooks);
    found[0].clone()
}

fn commands(list: &HookList) -> Vec<(HookSource, &str)> {
    list.hooks
        .iter()
        .map(|hook| (hook.source.clone(), hook.command.as_str()))
        .collect()
}

fn plugin_source(name: &str) -> HookSource {
    HookSource::Plugin {
        id: format!("{name}@local"),
    }
}

#[test]
fn an_if_condition_is_read_and_names_a_hook_apart_from_the_same_command_without_it() {
    let setup = Setup::new();
    setup.write_user_settings(
        json!({"hooks": {"PreToolUse": [{"matcher": "Bash", "hooks": [
            {"type": "command", "command": "guard"},
            {"type": "command", "command": "guard", "if": "Bash(git push*)"},
            {"type": "command", "command": "blank-if", "if": "  "},
            {"type": "command", "command": "numeric-if", "if": 3},
            {"type": "command", "command": "path-if", "if": "Edit(src/**)"},
        ]}], "Stop": [{"hooks": [
            {"type": "command", "command": "stop-if", "if": "Bash"},
        ]}]}}),
    );
    let first = setup.hooks();
    let conditions = first
        .hooks
        .iter()
        .map(|hook| (hook.command.as_str(), hook.condition.as_deref()))
        .collect::<Vec<_>>();
    assert_eq!(
        conditions,
        [
            ("guard", None),
            ("guard", Some("Bash(git push*)")),
            ("blank-if", None),
            ("path-if", Some("Edit(src/**)")),
            ("stop-if", Some("Bash")),
        ]
    );
    assert_ne!(first.hooks[0].key, first.hooks[1].key);
    let settings = setup.settings_path().display().to_string();
    assert_eq!(
        first.warnings,
        [
            format!(
                "{settings}: PreToolUse hook if must be text such as Bash(git push*); it is skipped"
            ),
            format!(
                "{settings}: PreToolUse hook if Edit(src/**) cannot be matched; the hook runs for \
                 every matching call"
            ),
            format!("{settings}: Stop hook if applies only to tool events; the hook never runs"),
        ]
    );

    setup.write_user_settings(
        json!({"hooks": {"PreToolUse": [{"matcher": "Bash", "hooks": [
            {"type": "command", "command": "guard"},
        ]}]}}),
    );
    assert_eq!(
        setup.hooks().hooks[0].key,
        first.hooks[0].key,
        "a hook without if keeps its identity"
    );
}

#[test]
fn async_and_async_rewake_are_read_and_name_a_hook_apart_from_the_same_command_without_them() {
    let setup = Setup::new();
    setup.write_user_settings(json!({"hooks": {"PostToolUse": [{"hooks": [
        {"type": "command", "command": "review"},
        {"type": "command", "command": "review", "async": true},
        {"type": "command", "command": "review", "async": true, "asyncRewake": true,
         "rewakeMessage": "Review:", "rewakeSummary": "Review found issues"},
        {"type": "command", "command": "quiet", "async": false, "rewakeSummary": "unused"},
        {"type": "command", "command": "loose", "async": "yes"},
        {"type": "command", "command": "blank", "asyncRewake": true, "rewakeMessage": " "},
    ]}]}}));

    let list = setup.hooks();

    let declared = list
        .hooks
        .iter()
        .map(|hook| (hook.command.as_str(), hook.background.clone()))
        .collect::<Vec<_>>();
    assert_eq!(
        declared,
        [
            ("review", None),
            ("review", Some(HookBackground::Async)),
            (
                "review",
                Some(HookBackground::AsyncRewake {
                    message: Some("Review:".into()),
                    summary: Some("Review found issues".into()),
                })
            ),
            ("quiet", None),
        ],
        "asyncRewake implies async, and the same command runs each way"
    );
    let keys = list.hooks[..3]
        .iter()
        .map(|hook| hook.key.as_str())
        .collect::<std::collections::HashSet<_>>();
    assert_eq!(keys.len(), 3);
    let settings = setup.settings_path().display().to_string();
    assert_eq!(
        list.warnings,
        [
            format!("{settings}: PostToolUse hook async must be true or false; it is skipped"),
            format!(
                "{settings}: PostToolUse hook rewakeMessage must be non-empty text; it is skipped"
            ),
        ]
    );

    setup.write_user_settings(json!({"hooks": {"PostToolUse": [{"hooks": [
        {"type": "command", "command": "review"},
    ]}]}}));
    assert_eq!(
        setup.hooks().hooks[0].key,
        list.hooks[0].key,
        "a hook that waits keeps its identity"
    );
}

#[test]
fn user_hooks_merge_the_top_level_and_the_folder_partition_and_need_no_trust() {
    let setup = Setup::new();
    let other = setup.directory.path().join("other");
    setup.write_user_settings(json!({
        "hooks": {"PreToolUse": [{"matcher": "Bash", "hooks": [
            {"type": "command", "command": "check-bash", "timeout": 30}
        ]}]},
        "projects": {
            setup.folder.to_str().unwrap(): {"hooks": hooks_of("Stop", Some("ignored"), &["finish"])},
            other.to_str().unwrap(): {"hooks": hooks_of("Stop", None, &["elsewhere"])},
        },
    }));

    let list = setup.hooks();

    assert!(list.warnings.is_empty(), "{:?}", list.warnings);
    assert_eq!(
        list.hooks,
        [
            ConfiguredHook {
                key: list.hooks[0].key.clone(),
                source: HookSource::User,
                file: setup.settings_path(),
                event: HookEvent::PreToolUse,
                matcher: Some("Bash".into()),
                command: "check-bash".into(),
                resolved_command: Some("check-bash".into()),
                sensitive_values: Vec::new(),
                condition: None,
                timeout_secs: 30,
                background: None,
                plugin: None,
                hash: None,
                trust: HookTrust::Trusted,
            },
            ConfiguredHook {
                key: list.hooks[1].key.clone(),
                source: HookSource::User,
                file: setup.settings_path(),
                event: HookEvent::Stop,
                matcher: None,
                command: "finish".into(),
                resolved_command: Some("finish".into()),
                sensitive_values: Vec::new(),
                condition: None,
                timeout_secs: DEFAULT_TIMEOUT_SECS,
                background: None,
                plugin: None,
                hash: None,
                trust: HookTrust::Trusted,
            },
        ]
    );
    assert!(list.hooks.iter().all(ConfiguredHook::may_run));
    assert_eq!(
        commands(&setup.settings().hooks(None)),
        [(HookSource::User, "check-bash")]
    );
}

#[test]
fn repository_hooks_come_only_from_the_hooks_of_both_claude_settings_files() {
    let setup = Setup::new();
    let shared = setup.write_repository_file(
        &setup.folder,
        "settings.json",
        json!({
            "permissions": {"allow": ["Bash"]},
            "hooks": hooks_of("PostToolUse", Some("Edit|Write"), &["format"]),
        }),
    );
    let local = setup.write_repository_file(
        &setup.folder,
        "settings.local.json",
        json!({
            "env": {"SECRET": "x"},
            "hooks": hooks_of("SessionStart", Some("startup"), &["greet"]),
        }),
    );

    let list = setup.hooks();

    assert!(list.warnings.is_empty(), "{:?}", list.warnings);
    let format = only(&list, "format");
    assert_eq!(format.source, HookSource::Repository);
    assert_eq!(format.file, shared);
    assert_eq!(format.event, HookEvent::PostToolUse);
    assert_eq!(format.matcher.as_deref(), Some("Edit|Write"));
    assert_eq!(format.trust, HookTrust::Pending);
    assert!(format.hash.is_some());
    assert!(!format.may_run());
    let greet = only(&list, "greet");
    assert_eq!(greet.file, local);
    assert_eq!(greet.matcher.as_deref(), Some("startup"));
    assert!(
        setup
            .settings()
            .read(Some(&setup.folder))
            .project
            .rules
            .is_empty()
    );
}

#[test]
fn enabled_plugin_hooks_substitute_plugin_placeholders_and_wait_for_missing_configuration() {
    let setup = Setup::new();
    let plugin = setup.install_plugin(
        "guard",
        json!({
            "name": "guard",
            "userConfig": {
                "token": {"type": "string", "title": "Token"},
                "level": {"type": "string", "title": "Level", "default": "high"},
            },
            "hooks": hooks_of("Stop", None, &["${CLAUDE_PLUGIN_DATA}/stop ${user_config.token}"]),
        }),
        &[(
            "hooks/hooks.json",
            &hooks_json(hooks_of(
                "PreToolUse",
                Some("Bash"),
                &["\"${CLAUDE_PLUGIN_ROOT}/check.sh\" ${TICKLOOP_PLUGIN_ROOT} ${TICKLOOP_PLUGIN_DATA} ${user_config.level} $CLAUDE_PROJECT_DIR ${CLAUDE_PROJECT_DIR}"],
            )),
        )],
    );
    setup.install_plugin(
        "quiet",
        json!({"name": "quiet"}),
        &[(
            "hooks/hooks.json",
            &hooks_json(hooks_of("Stop", None, &["quiet"])),
        )],
    );
    setup.plugins().set_enabled("quiet", false).unwrap();
    let data = setup.plugins().data_directory("guard");

    let list = setup.hooks();

    assert!(list.warnings.is_empty(), "{:?}", list.warnings);
    assert_eq!(list.hooks.len(), 2, "{:#?}", list.hooks);
    let check = &list.hooks[0];
    assert_eq!(check.source, plugin_source("guard"));
    assert_eq!(check.file, plugin.root.join("hooks/hooks.json"));
    assert_eq!(
        check.resolved_command.as_deref(),
        Some(
            format!(
                "\"{root}/check.sh\" {root} {data} high $CLAUDE_PROJECT_DIR ${{CLAUDE_PROJECT_DIR}}",
                root = plugin.root.display(),
                data = data.display()
            )
            .as_str()
        )
    );
    assert_eq!(
        check.plugin,
        Some(HookPluginDirectories {
            root: plugin.root.clone(),
            data: data.clone(),
        })
    );
    assert_eq!(check.trust, HookTrust::Pending);
    assert!(data.is_dir());
    let stop = &list.hooks[1];
    assert_eq!(stop.file, plugin.root.join(".claude-plugin/plugin.json"));
    assert!(stop.needs_configuration());
    assert_eq!(
        stop.command,
        "${CLAUDE_PLUGIN_DATA}/stop ${user_config.token}"
    );
}

#[test]
fn unsupported_types_events_and_fields_skip_only_their_hook_with_a_warning() {
    let setup = Setup::new();
    setup.write_user_settings(json!({"hooks": {
        "PreToolUse": [{"matcher": "Bash", "hooks": [
            {"type": "prompt", "prompt": "Is this safe?"},
            {"type": "command", "command": "just-once", "once": true},
            {"type": "command", "command": "kept", "statusMessage": "Checking"},
        ]}],
        "Notification": [{"hooks": [{"type": "command", "command": "notify"}]}],
        "Stop": [{"hooks": [
            {"type": "http", "url": "https://example.com/hook"},
            {"type": "command", "command": "with-args", "args": ["a"]},
            {"type": "command", "command": "", "timeout": 5},
        ]}],
    }}));

    let list = setup.hooks();

    assert_eq!(commands(&list), [(HookSource::User, "kept")]);
    let settings = setup.settings_path().display().to_string();
    assert_eq!(
        list.warnings,
        [
            format!("{settings}: PreToolUse hook type prompt is not supported; it is skipped"),
            format!("{settings}: PreToolUse hook field once is not supported; the hook is skipped"),
            format!("{settings}: hook event Notification is not supported; its hooks are skipped"),
            format!("{settings}: Stop hook type http is not supported; it is skipped"),
            format!("{settings}: Stop hook field args is not supported; the hook is skipped"),
            format!("{settings}: Stop hook needs a command; it is skipped"),
        ]
    );
}

#[test]
fn both_repository_settings_files_report_unsupported_hooks_and_keep_supported_ones() {
    for name in ["settings.json", "settings.local.json"] {
        let setup = Setup::new();
        let path = setup.write_repository_file(
            &setup.folder,
            name,
            json!({"hooks": {
                "Notification": [{"hooks": [{"type": "command", "command": "ignored"}]}],
                "Stop": [{"hooks": [
                    {"type": "prompt", "prompt": "check"},
                    {"type": "command", "command": "ignored", "shell": "bash"},
                    {"type": "command", "command": "kept"}
                ]}]
            }}),
        );
        let list = setup.hooks();
        assert_eq!(commands(&list), [(HookSource::Repository, "kept")]);
        assert_eq!(list.hooks[0].file, path);
        for unsupported in ["event Notification", "type prompt", "field shell"] {
            assert!(
                list.warnings
                    .iter()
                    .any(|warning| warning.contains(name) && warning.contains(unsupported)),
                "{:?}",
                list.warnings
            );
        }
    }
}

#[test]
fn identical_commands_collapse_within_a_source_but_not_across_plugins() {
    let setup = Setup::new();
    setup.write_user_settings(json!({
        "hooks": {"PreToolUse": [
            {"matcher": "Bash", "hooks": [
                {"type": "command", "command": "same"},
                {"type": "command", "command": "same", "timeout": 5},
            ]},
            {"matcher": "Bash", "hooks": [{"type": "command", "command": "same"}]},
            {"matcher": "Edit", "hooks": [{"type": "command", "command": "same"}]},
        ]},
        "projects": {setup.folder.to_str().unwrap(): {"hooks": hooks_of("PreToolUse", Some("Bash"), &["same"])}},
    }));
    setup.write_repository_file(
        &setup.folder,
        "settings.json",
        json!({"hooks": hooks_of("PreToolUse", Some("Bash"), &["same"])}),
    );
    setup.write_repository_file(
        &setup.folder,
        "settings.local.json",
        json!({"hooks": hooks_of("PreToolUse", Some("Bash"), &["same"])}),
    );
    for name in ["first", "second"] {
        setup.install_plugin(
            name,
            json!({"name": name}),
            &[(
                "hooks/hooks.json",
                &hooks_json(hooks_of("PreToolUse", Some("Bash"), &["same", "same"])),
            )],
        );
    }

    let list = setup.hooks();

    assert_eq!(
        list.hooks
            .iter()
            .map(|hook| (hook.source.clone(), hook.matcher.as_deref().unwrap()))
            .collect::<Vec<_>>(),
        [
            (HookSource::User, "Bash"),
            (HookSource::User, "Edit"),
            (HookSource::Repository, "Bash"),
            (plugin_source("first"), "Bash"),
            (plugin_source("second"), "Bash"),
        ]
    );
}

#[test]
fn repository_trust_is_kept_per_folder_and_lapses_when_claude_hooks_change() {
    let setup = Setup::new();
    let other = setup.directory.path().join("other");
    for folder in [&setup.folder, &other] {
        setup.write_repository_file(
            folder,
            "settings.json",
            json!({"hooks": hooks_of("PreToolUse", Some("Bash"), &[".claude/hooks/check.sh"])}),
        );
        fs::create_dir_all(folder.join(".claude/hooks")).unwrap();
        fs::write(folder.join(".claude/hooks/check.sh"), "exit 0\n").unwrap();
    }

    let pending = only(&setup.hooks(), ".claude/hooks/check.sh");
    assert_eq!(pending.trust, HookTrust::Pending);
    setup.trust(&pending);
    let trusted = only(&setup.hooks(), ".claude/hooks/check.sh");
    assert_eq!(trusted.trust, HookTrust::Trusted);
    assert!(trusted.may_run());
    assert_eq!(
        only(
            &setup.settings().hooks(Some(&other)),
            ".claude/hooks/check.sh"
        )
        .trust,
        HookTrust::Pending
    );

    fs::write(setup.folder.join(".claude/hooks/check.sh"), "exit 2\n").unwrap();
    let modified = only(&setup.hooks(), ".claude/hooks/check.sh");
    assert_eq!(modified.trust, HookTrust::Modified);
    assert_ne!(modified.hash, trusted.hash);
    assert_eq!(modified.key, trusted.key);

    setup.trust(&modified);
    fs::write(setup.folder.join(".claude/hooks/helper.sh"), "exit 0\n").unwrap();
    assert_eq!(
        only(&setup.hooks(), ".claude/hooks/check.sh").trust,
        HookTrust::Modified
    );

    setup.write_repository_file(
        &setup.folder,
        "settings.json",
        json!({"hooks": hooks_of("PreToolUse", Some("Bash|Edit"), &[".claude/hooks/check.sh"])}),
    );
    assert_eq!(
        only(&setup.hooks(), ".claude/hooks/check.sh").trust,
        HookTrust::Pending
    );
}

#[test]
fn hooks_of_every_source_can_be_disabled_and_enabled_but_only_hashed_ones_trusted() {
    let setup = Setup::new();
    setup.write_user_settings(json!({"hooks": hooks_of("Stop", None, &["mine"])}));
    setup.install_plugin(
        "guard",
        json!({"name": "guard"}),
        &[(
            "hooks/hooks.json",
            &hooks_json(hooks_of("Stop", None, &["theirs"])),
        )],
    );
    let mine = only(&setup.hooks(), "mine");
    let theirs = only(&setup.hooks(), "theirs");

    assert!(matches!(
        setup.change(
            &mine,
            HookTrustAction::Trust {
                hash: "anything".into()
            }
        ),
        Err(SettingsStoreError::Storage(_))
    ));
    setup.change(&mine, HookTrustAction::Disable).unwrap();
    setup.change(&theirs, HookTrustAction::Disable).unwrap();
    let list = setup.hooks();
    assert_eq!(only(&list, "mine").trust, HookTrust::Disabled);
    assert_eq!(only(&list, "theirs").trust, HookTrust::Disabled);
    assert!(!only(&list, "mine").may_run());

    setup.trust(&theirs);
    assert_eq!(only(&setup.hooks(), "theirs").trust, HookTrust::Disabled);
    setup.change(&mine, HookTrustAction::Enable).unwrap();
    setup.change(&theirs, HookTrustAction::Enable).unwrap();
    let list = setup.hooks();
    assert_eq!(only(&list, "mine").trust, HookTrust::Trusted);
    assert_eq!(only(&list, "theirs").trust, HookTrust::Trusted);
}

#[test]
fn a_plugin_update_lapses_its_hook_trust_and_uninstalling_forgets_its_records() {
    let setup = Setup::new();
    let hooks = hooks_json(hooks_of(
        "PreToolUse",
        None,
        &["\"${CLAUDE_PLUGIN_ROOT}/check.sh\""],
    ));
    let install = |script: &str| {
        setup.install_plugin(
            "guard",
            json!({"name": "guard"}),
            &[("hooks/hooks.json", &hooks), ("check.sh", script)],
        )
    };
    install("exit 0\n");
    let first = setup.hooks().hooks.remove(0);
    setup.trust(&first);
    assert_eq!(setup.hooks().hooks[0].trust, HookTrust::Trusted);

    install("exit 2\n");
    let updated = setup.hooks().hooks.remove(0);
    assert_eq!(updated.trust, HookTrust::Modified);
    assert_eq!(updated.key, first.key);
    setup.trust(&updated);

    setup.plugins().uninstall("guard", false).unwrap();
    assert!(setup.settings().forget_plugin_hooks("guard@local").unwrap());
    assert_eq!(setup.user_settings()["hookTrust"]["plugins"], json!({}));
    assert!(!setup.settings().forget_plugin_hooks("guard@local").unwrap());
    install("exit 2\n");
    assert_eq!(setup.hooks().hooks[0].trust, HookTrust::Pending);
}

#[test]
fn a_plugin_counts_its_hooks_never_confirmed_or_changed_since_whether_or_not_enabled() {
    let setup = Setup::new();
    let hooks = hooks_json(json!({
        "PreToolUse": [{"hooks": [
            {"type": "command", "command": "\"${CLAUDE_PLUGIN_ROOT}/check.sh\""},
            {"type": "command", "command": "second"},
            {"type": "command", "command": "third"},
            {"type": "prompt", "prompt": "Is this safe?"},
        ]}],
        "Notification": [{"hooks": [{"type": "command", "command": "notify"}]}],
    }));
    let install = |script: &str| {
        setup.install_plugin(
            "guard",
            json!({"name": "guard"}),
            &[("hooks/hooks.json", &hooks), ("check.sh", script)],
        )
    };
    let plugin = install("exit 0\n");
    assert_eq!(setup.settings().plugin_hooks_to_confirm(&plugin), 3);

    setup.trust(&only(&setup.hooks(), "\"${CLAUDE_PLUGIN_ROOT}/check.sh\""));
    setup
        .change(&only(&setup.hooks(), "third"), HookTrustAction::Disable)
        .unwrap();
    assert_eq!(setup.settings().plugin_hooks_to_confirm(&plugin), 1);

    let updated = install("exit 2\n");
    assert_eq!(setup.settings().plugin_hooks_to_confirm(&updated), 2);

    let disabled = setup.plugins().set_enabled("guard", false).unwrap();
    assert_eq!(setup.settings().plugin_hooks_to_confirm(&disabled), 2);
    let plain = setup.install_plugin("plain", json!({"name": "plain"}), &[]);
    assert_eq!(setup.settings().plugin_hooks_to_confirm(&plain), 0);
}

#[test]
fn the_plugin_manifest_names_exactly_the_hook_events_tickloop_runs() {
    assert_eq!(
        tick_plugins::HOOK_EVENTS.to_vec(),
        HookEvent::ALL.map(HookEvent::name).to_vec()
    );
}

#[test]
fn trust_writes_check_the_revision_and_keep_unrelated_settings() {
    let setup = Setup::new();
    setup.write_user_settings(json!({"theme": "dark"}));
    setup.write_repository_file(
        &setup.folder,
        "settings.json",
        json!({"hooks": hooks_of("Stop", None, &["finish"])}),
    );
    let before = setup.hooks();
    let hook = before.hooks[0].clone();
    let trust = [HookTrustChange {
        source: HookSource::Repository,
        key: hook.key.clone(),
        action: HookTrustAction::Trust {
            hash: hook.hash.clone().unwrap(),
        },
    }];

    setup
        .settings()
        .write_hook_trust(Some(&setup.folder), &trust, Some(&before.revision))
        .unwrap();
    let after = setup.hooks();
    assert_ne!(after.revision, before.revision);
    assert!(matches!(
        setup
            .settings()
            .write_hook_trust(Some(&setup.folder), &trust, Some(&before.revision)),
        Err(SettingsStoreError::Conflict)
    ));
    setup
        .settings()
        .write_hook_trust(Some(&setup.folder), &trust, None)
        .unwrap();
    assert_eq!(setup.hooks().revision, after.revision);
    assert_eq!(setup.user_settings()["theme"], "dark");
}

#[test]
fn unreadable_trust_records_leave_hooks_unconfirmed_with_a_warning() {
    let setup = Setup::new();
    let saved = json!({"hookTrust": {"plugins": "everything"}});
    setup.write_user_settings(saved.clone());
    setup.install_plugin(
        "guard",
        json!({"name": "guard"}),
        &[(
            "hooks/hooks.json",
            &hooks_json(hooks_of("Stop", None, &["theirs"])),
        )],
    );

    let list = setup.hooks();

    assert_eq!(list.hooks[0].trust, HookTrust::Pending);
    assert_eq!(
        list.warnings,
        [format!(
            "settings file {}: hookTrust.plugins must be an object; its hook trust records are ignored",
            setup.settings_path().display()
        )]
    );
    assert!(
        setup
            .change(&list.hooks[0], HookTrustAction::Disable)
            .is_err()
    );
    assert_eq!(setup.user_settings(), saved);
}

fn trust_after(setup: &Setup, command: &str, change: impl FnOnce()) -> HookTrust {
    setup.trust(&only(&setup.hooks(), command));
    change();
    only(&setup.hooks(), command).trust
}

#[test]
fn a_plugin_fingerprint_covers_everything_it_shipped_but_not_files_made_after_install() {
    let setup = Setup::new();
    let plugin = setup.install_plugin(
        "guard",
        json!({"name": "guard"}),
        &[
            (
                "hooks/hooks.json",
                &hooks_json(hooks_of("Stop", None, &["guard"])),
            ),
            (".venv/lib/site.pth", "import shipped\n"),
            (
                "hooks/__pycache__/check.cpython-312.pyc",
                "shipped bytecode",
            ),
            ("pkg.egg-info/PKG-INFO", "Name: guard\n"),
        ],
    );
    let root = &plugin.root;

    for shipped in [
        ".venv/lib/site.pth",
        "hooks/__pycache__/check.cpython-312.pyc",
        "pkg.egg-info/PKG-INFO",
    ] {
        assert_eq!(
            trust_after(&setup, "guard", || fs::write(root.join(shipped), "changed")
                .unwrap()),
            HookTrust::Modified,
            "{shipped}"
        );
    }
    assert_eq!(
        trust_after(&setup, "guard", || fs::remove_file(
            root.join(".venv/lib/site.pth")
        )
        .unwrap()),
        HookTrust::Modified
    );
    assert_eq!(
        trust_after(&setup, "guard", || {
            fs::create_dir_all(root.join("__pycache__")).unwrap();
            fs::write(root.join("__pycache__/run.pyc"), "generated").unwrap();
            fs::write(root.join("hooks/__pycache__/new.pyc"), "generated").unwrap();
        }),
        HookTrust::Trusted
    );
    assert_eq!(
        trust_after(&setup, "guard", || fs::write(
            root.join(SHIPPED_FILES_RECORD),
            "[]"
        )
        .unwrap()),
        HookTrust::Modified,
        "a record that leaves out shipped files"
    );
}

#[test]
fn a_plugin_installed_without_a_record_of_its_files_covers_every_file() {
    let setup = Setup::new();
    let plugin = setup.install_plugin(
        "guard",
        json!({"name": "guard"}),
        &[(
            "hooks/hooks.json",
            &hooks_json(hooks_of("Stop", None, &["guard"])),
        )],
    );
    fs::remove_file(plugin.root.join(SHIPPED_FILES_RECORD)).unwrap();

    assert_eq!(
        trust_after(&setup, "guard", || {
            fs::create_dir_all(plugin.root.join("__pycache__")).unwrap();
            fs::write(plugin.root.join("__pycache__/run.pyc"), "generated").unwrap();
        }),
        HookTrust::Modified
    );
}

#[test]
fn content_behind_symlinks_counts_for_the_fingerprint() {
    let setup = Setup::new();
    setup.write_repository_file(
        &setup.folder,
        "settings.json",
        json!({"hooks": hooks_of("Stop", None, &[".claude/hooks/check.sh"])}),
    );
    let hooks = setup.folder.join(".claude/hooks");
    let scripts = setup.folder.join("scripts");
    fs::create_dir_all(&hooks).unwrap();
    fs::create_dir_all(scripts.join("lib")).unwrap();
    fs::write(scripts.join("check.sh"), "exit 0\n").unwrap();
    std::os::unix::fs::symlink("../../scripts/check.sh", hooks.join("check.sh")).unwrap();
    std::os::unix::fs::symlink("../../scripts/lib", hooks.join("lib")).unwrap();
    std::os::unix::fs::symlink("../../scripts/missing.sh", hooks.join("later.sh")).unwrap();
    std::os::unix::fs::symlink(".", hooks.join("loop")).unwrap();
    std::os::unix::fs::symlink("../../scripts", scripts.join("lib/up")).unwrap();
    let command = ".claude/hooks/check.sh";

    assert_eq!(
        trust_after(&setup, command, || fs::write(
            scripts.join("check.sh"),
            "exit 2\n"
        )
        .unwrap()),
        HookTrust::Modified
    );
    assert_eq!(
        trust_after(&setup, command, || fs::write(
            scripts.join("lib/util.sh"),
            "x\n"
        )
        .unwrap()),
        HookTrust::Modified
    );
    assert_eq!(
        trust_after(&setup, command, || fs::write(
            scripts.join("missing.sh"),
            "x\n"
        )
        .unwrap()),
        HookTrust::Modified
    );
    assert_eq!(
        trust_after(&setup, command, || {}),
        HookTrust::Trusted,
        "cycles hash the same each time"
    );
}

const HOOK_SECRET_SENTINEL: &str = "HOOK_SECRET_SENTINEL";

#[test]
fn a_plugin_hook_carries_its_sensitive_values_and_views_get_it_without_them() {
    use tickloop_code_settings::plugin_config::PluginUserConfig;
    let setup = Setup::new();
    let command = "check ${user_config.token} ${user_config.level}";
    let plugin = setup.install_plugin(
        "guard",
        json!({
            "name": "guard",
            "userConfig": {
                "token": {"type": "string", "title": "Token", "sensitive": true},
                "unused": {"type": "string", "title": "Unused", "sensitive": true},
                "level": {"type": "string", "title": "Level"},
            },
        }),
        &[(
            "hooks/hooks.json",
            &hooks_json(hooks_of("PreToolUse", None, &[command])),
        )],
    );
    PluginUserConfig::load(&setup.config_root, &plugin.manifest)
        .unwrap()
        .save(
            &setup.config_root,
            &plugin.manifest,
            &serde_json::from_value(json!({
                "token": HOOK_SECRET_SENTINEL,
                "unused": "other-secret",
                "level": "high",
            }))
            .unwrap(),
        )
        .unwrap();

    let hook = only(&setup.hooks(), command);
    assert_eq!(
        hook.resolved_command.as_deref(),
        Some("check HOOK_SECRET_SENTINEL high"),
        "the hook runs with the value"
    );
    assert_eq!(hook.sensitive_values, [HOOK_SECRET_SENTINEL]);

    let shown = hook.for_views();
    assert_eq!(
        shown.resolved_command.as_deref(),
        Some("check [redacted] high")
    );
    assert!(shown.sensitive_values.is_empty());
    let wire = serde_json::to_string(&shown).unwrap();
    assert!(!wire.contains(HOOK_SECRET_SENTINEL), "{wire}");
}

const MOVED_PAYLOAD_COMMAND: &str =
    "test ! -f .claude/hooks/a/payload.sh || sh .claude/hooks/a/payload.sh";

/// A Folder whose hook runs `.claude/hooks/a/payload.sh` only once it exists, with the payload
/// beside `a/` for now; returns `.claude/hooks`.
fn payload_beside_its_directory(setup: &Setup) -> PathBuf {
    setup.write_repository_file(
        &setup.folder,
        "settings.json",
        json!({"hooks": hooks_of("Stop", None, &[MOVED_PAYLOAD_COMMAND])}),
    );
    let scripts = setup.folder.join(".claude/hooks");
    fs::create_dir_all(scripts.join("a")).unwrap();
    fs::write(scripts.join("a/anchor"), "").unwrap();
    fs::write(scripts.join("payload.sh"), "printf changed").unwrap();
    scripts
}

#[test]
fn moving_a_script_into_a_subdirectory_lapses_repository_trust() {
    let setup = Setup::new();
    let scripts = payload_beside_its_directory(&setup);

    assert_eq!(
        trust_after(&setup, MOVED_PAYLOAD_COMMAND, || fs::rename(
            scripts.join("payload.sh"),
            scripts.join("a/payload.sh")
        )
        .unwrap()),
        HookTrust::Modified
    );
}

#[test]
fn moving_a_file_into_a_subdirectory_of_a_plugin_without_a_record_lapses_its_trust() {
    let setup = Setup::new();
    let plugin = setup.install_plugin(
        "guard",
        json!({"name": "guard"}),
        &[
            (
                "hooks/hooks.json",
                &hooks_json(hooks_of("Stop", None, &["guard"])),
            ),
            ("z/anchor", ""),
            ("zz.sh", "printf changed"),
        ],
    );
    fs::remove_file(plugin.root.join(SHIPPED_FILES_RECORD)).unwrap();

    assert_eq!(
        trust_after(&setup, "guard", || fs::rename(
            plugin.root.join("zz.sh"),
            plugin.root.join("z/zz.sh")
        )
        .unwrap()),
        HookTrust::Modified
    );
}

/// The hash the hook of [`payload_beside_its_directory`] had under the previous fingerprint
/// encoding, which named only each entry's last component and marked no directory's end.
const PREVIOUS_ENCODING_HASH: &str = "cabfd4617c6a450051066ba76b85b3bb23fa12d63519f8f70eb4733fa6f577c7/sha256:c9dbd6f4a2feaa88188b4b546e66542171b9be82933b9a99fc4e4d9495b91ec3";

#[test]
fn trust_confirmed_under_the_previous_fingerprint_encoding_lapses() {
    let setup = Setup::new();
    payload_beside_its_directory(&setup);
    let hook = only(&setup.hooks(), MOVED_PAYLOAD_COMMAND);
    setup.write_user_settings(json!({"projects": {
        setup.folder.to_str().unwrap(): {"hookTrust": {
            hook.key.clone(): {"trustedHash": PREVIOUS_ENCODING_HASH}
        }}
    }}));

    let hook = only(&setup.hooks(), MOVED_PAYLOAD_COMMAND);
    assert_ne!(hook.hash.as_deref(), Some(PREVIOUS_ENCODING_HASH));
    assert_eq!(hook.trust, HookTrust::Modified);
}

/// Takes every permission from `path` until the guard drops.
fn without_permissions(path: &Path) -> impl Drop + use<> {
    use std::os::unix::fs::PermissionsExt;
    struct Restore(PathBuf, fs::Permissions);
    impl Drop for Restore {
        fn drop(&mut self) {
            let _ = fs::set_permissions(&self.0, self.1.clone());
        }
    }
    let saved = fs::metadata(path).unwrap().permissions();
    fs::set_permissions(path, fs::Permissions::from_mode(0o000)).unwrap();
    Restore(path.to_owned(), saved)
}

#[test]
fn a_read_failure_behind_a_linked_directory_skips_the_hooks_as_in_a_direct_directory() {
    let setup = Setup::new();
    setup.write_repository_file(
        &setup.folder,
        "settings.json",
        json!({"hooks": hooks_of("Stop", None, &[".claude/hooks/z/check.sh"])}),
    );
    let hooks = setup.folder.join(".claude/hooks");
    let scripts = setup.folder.join("scripts");
    fs::create_dir_all(&hooks).unwrap();
    fs::create_dir_all(&scripts).unwrap();
    fs::write(scripts.join("check.sh"), "exit 0\n").unwrap();
    fs::write(scripts.join("helper.sh"), "exit 0\n").unwrap();
    std::os::unix::fs::symlink("../../scripts", hooks.join("z")).unwrap();
    assert_eq!(setup.hooks().hooks.len(), 1);

    let _restore = without_permissions(&scripts.join("helper.sh"));
    let list = setup.hooks();

    assert!(list.hooks.is_empty(), "{:#?}", list.hooks);
    assert!(
        list.warnings
            .iter()
            .any(|warning| warning.contains("the Folder's hooks are skipped")),
        "{:?}",
        list.warnings
    );
}

#[test]
fn a_link_whose_target_cannot_be_opened_counts_as_a_fixed_marker() {
    let setup = Setup::new();
    let command = ".claude/hooks/check.sh";
    setup.write_repository_file(
        &setup.folder,
        "settings.json",
        json!({"hooks": hooks_of("Stop", None, &[command])}),
    );
    let hooks = setup.folder.join(".claude/hooks");
    let locked = setup.folder.join("locked");
    fs::create_dir_all(&hooks).unwrap();
    fs::create_dir_all(&locked).unwrap();
    fs::write(locked.join("inside.sh"), "exit 0\n").unwrap();
    fs::write(hooks.join("check.sh"), "exit 0\n").unwrap();
    std::os::unix::fs::symlink("../../locked", hooks.join("locked")).unwrap();
    let _restore = without_permissions(&locked);

    let list = setup.hooks();
    assert!(list.warnings.is_empty(), "{:?}", list.warnings);
    assert_eq!(
        trust_after(&setup, command, || {}),
        HookTrust::Trusted,
        "an unreachable target hashes the same each time"
    );
}
