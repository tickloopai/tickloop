use std::{collections::BTreeMap, path::Path};
use tickloop_agent_policy::{
    ProjectRuleChanges,
    settings::{MemorySettingsStore, SettingsStore},
};
use tickloop_agent_protocol::{Decision, PermissionRule};

#[test]
fn persistent_permissions_are_partitioned_and_stale_edits_are_rejected() {
    let store = MemorySettingsStore::default();
    let project = Path::new("/projects/one");
    let initial = store.read(project.into()).project;
    let rule = PermissionRule {
        id: "00000000-0000-0000-0000-000000000001".parse().unwrap(),
        decision: Decision::Allow,
        tool: "Read".into(),
        specifier: None,
        input: None,
        loop_id: None,
        persistent: true,
    };
    let changes = BTreeMap::from([(
        project.to_owned(),
        ProjectRuleChanges {
            added: vec![rule],
            ..Default::default()
        },
    )]);
    store
        .write_permissions(&changes, Some((project, &initial.revision)))
        .unwrap();
    let saved = store.read(Some(project)).project;
    assert_eq!(saved.rules.len(), 1);
    store
        .write_permissions(&changes, Some((project, &saved.revision)))
        .unwrap();
    assert_eq!(
        store.read(Some(project)).project.revision,
        saved.revision,
        "an identical save must not invalidate another editor"
    );
    assert!(
        store
            .read(Some(Path::new("/projects/two")))
            .project
            .rules
            .is_empty()
    );
    assert!(
        store
            .write_permissions(&changes, Some((project, &initial.revision)))
            .is_err()
    );
}

#[test]
fn new_chat_preferences_and_auto_changes_survive_store_reads() {
    use tickloop_agent_policy::settings::SettingsUpdate;
    use tickloop_agent_protocol::{AutoReviewerPreference, ExecutionMode};
    let store = MemorySettingsStore::default();
    store.write(SettingsUpdate::TaskTools(true)).unwrap();
    store
        .write(SettingsUpdate::ExecutionMode(ExecutionMode::Plan))
        .unwrap();
    let (_, revision) = store.read(None).auto.unwrap();
    store
        .write(SettingsUpdate::AutoReviewer {
            expected: revision.clone(),
            reviewer: AutoReviewerPreference::Jev,
        })
        .unwrap();
    let saved = store.read(None);
    assert!(saved.task_tools_enabled);
    assert_eq!(saved.execution_mode, Some(ExecutionMode::Plan));
    let (auto, current_revision) = saved.auto.unwrap();
    assert_eq!(auto.reviewer, AutoReviewerPreference::Jev);
    store
        .write(SettingsUpdate::AutoReviewer {
            expected: current_revision.clone(),
            reviewer: AutoReviewerPreference::Jev,
        })
        .unwrap();
    assert_eq!(store.read(None).auto.unwrap().1, current_revision);
    assert!(
        store
            .write(SettingsUpdate::AutoReviewer {
                expected: revision,
                reviewer: AutoReviewerPreference::Llm
            })
            .is_err()
    );
}
