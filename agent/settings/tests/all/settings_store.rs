use std::{collections::BTreeMap, sync::Arc};

use tickloop_agent_protocol::{AutoReviewerPreference, Decision, ExecutionMode, PermissionRule};
use tickloop_agent_settings::UserSettings;
use tickloop_harness::policy::{
    ProjectRuleChanges,
    settings::{MemorySettingsStore, SettingsStore, SettingsStoreError, SettingsUpdate},
};

#[test]
fn file_and_memory_stores_keep_preferences_and_partitioned_permission_edits() {
    let directory = tempfile::tempdir().unwrap();
    let project = directory.path().join("project");
    let other = directory.path().join("other");
    let stores: [Arc<dyn SettingsStore>; 2] = [
        Arc::new(MemorySettingsStore::default()),
        Arc::new(UserSettings::in_root(directory.path())),
    ];
    for store in stores {
        store.write(SettingsUpdate::TaskTools(true)).unwrap();
        store
            .write(SettingsUpdate::ExecutionMode(ExecutionMode::Plan))
            .unwrap();
        let (_, auto_revision) = store.read(None).auto.unwrap();
        store
            .write(SettingsUpdate::AutoReviewer {
                expected: auto_revision.clone(),
                reviewer: AutoReviewerPreference::Jev,
            })
            .unwrap();
        let first = store.read(Some(&project));
        let changes = BTreeMap::from([(
            project.clone(),
            ProjectRuleChanges {
                added: vec![PermissionRule {
                    id: "00000000-0000-0000-0000-000000000001".parse().unwrap(),
                    tool: "Read".into(),
                    decision: Decision::Allow,
                    specifier: None,
                    input: None,
                    loop_id: None,
                    persistent: true,
                }],
                ..Default::default()
            },
        )]);
        store
            .write_permissions(&changes, Some((&project, &first.project.revision)))
            .unwrap();
        let saved = store.read(Some(&project));
        assert_eq!(saved.project.rules[0].tool, "Read");
        assert!(store.read(Some(&other)).project.rules.is_empty());
        assert!(saved.task_tools_enabled);
        assert_eq!(saved.execution_mode, Some(ExecutionMode::Plan));
        assert_eq!(saved.auto.unwrap().0.reviewer, AutoReviewerPreference::Jev);
        assert!(matches!(
            store.write_permissions(&changes, Some((&project, &first.project.revision))),
            Err(SettingsStoreError::Conflict)
        ));
        assert!(matches!(
            store.write(SettingsUpdate::AutoReviewer {
                expected: auto_revision,
                reviewer: AutoReviewerPreference::Llm
            }),
            Err(SettingsStoreError::Conflict)
        ));
        store
            .write_permissions(&changes, Some((&project, &saved.project.revision)))
            .unwrap();
        assert_eq!(
            store.read(Some(&project)).project.revision,
            saved.project.revision
        );
    }
}

#[test]
fn file_and_memory_stores_preserve_thinking_choices_across_effort_changes() {
    use tickloop_agent_protocol::ReasoningEffort;
    let directory = tempfile::tempdir().unwrap();
    let stores: [Arc<dyn SettingsStore>; 2] = [
        Arc::new(UserSettings::in_root(directory.path())),
        Arc::new(MemorySettingsStore::default()),
    ];
    for store in stores {
        store
            .write(SettingsUpdate::ReasoningEffort(Some(ReasoningEffort::Off)))
            .unwrap();
        assert!(!store.read(None).thinking_enabled);
        assert_eq!(store.read(None).reasoning_effort, None);
        store
            .write(SettingsUpdate::ReasoningEffort(Some(ReasoningEffort::Low)))
            .unwrap();
        assert!(!store.read(None).thinking_enabled);
        store
            .write(SettingsUpdate::Thinking {
                enabled: true,
                effort: None,
            })
            .unwrap();
        store
            .write(SettingsUpdate::ReasoningEffort(Some(ReasoningEffort::Off)))
            .unwrap();
        assert!(
            store.read(None).thinking_enabled,
            "an explicit thinking choice takes precedence over the legacy off effort"
        );
    }
}
