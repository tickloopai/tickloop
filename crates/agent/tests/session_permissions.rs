#[allow(dead_code)]
#[path = "../src/permissions.rs"]
mod permissions;
mod tools {
    pub use tickloop_agent::tools::*;
}

#[path = "../src/runtime/session_permissions.rs"]
mod session_permissions;

use permissions::SessionPermissionDecision;
use serde_json::json;
use session_permissions::{
    LoopInheritance, PermissionOwner, SessionPermissionContext, SessionPermissionError,
    SessionPermissionOperation, WorkspaceScope,
};
use tools::Workspace;
use uuid::Uuid;

fn owner(chat_id: Uuid, loop_id: Uuid) -> PermissionOwner {
    PermissionOwner::new(chat_id, loop_id).unwrap()
}

fn make_workspace(
    root: &std::path::Path,
    additional_directories: Vec<std::path::PathBuf>,
) -> Workspace {
    Workspace {
        workdir: root.to_path_buf(),
        additional_directories,
    }
}

fn operation(
    workspace: &Workspace,
    path: &std::path::Path,
    content: &str,
) -> SessionPermissionOperation {
    let prepared = tickloop_agent::tools::prepare(
        workspace,
        "Write",
        &json!({"content": content, "file_path": path}),
    )
    .unwrap();
    SessionPermissionOperation::from_prepared(&prepared, workspace).unwrap()
}

#[test]
fn exact_operation_workspace_and_owner_are_required() {
    let root = tempfile::tempdir().unwrap();
    let additional = tempfile::tempdir().unwrap();
    let expanded = tempfile::tempdir().unwrap();
    let source = owner(Uuid::new_v4(), Uuid::new_v4());
    let workspace = make_workspace(root.path(), vec![additional.path().to_path_buf()]);
    let permitted = operation(&workspace, &root.path().join("report.txt"), "approved");
    let mut context = SessionPermissionContext::default();

    context
        .record(source, permitted.clone(), SessionPermissionDecision::Allow)
        .unwrap();

    let evaluation = context.evaluate(source, &permitted);
    assert_eq!(evaluation.decision, Some(SessionPermissionDecision::Allow));
    assert!(context.revalidate(&evaluation.token, source, &permitted));

    let changed_input = operation(&workspace, &root.path().join("report.txt"), "changed");
    assert_eq!(context.evaluate(source, &changed_input).decision, None);
    assert!(!context.revalidate(&evaluation.token, source, &changed_input));

    let expanded_workspace = make_workspace(
        root.path(),
        vec![
            additional.path().to_path_buf(),
            expanded.path().to_path_buf(),
        ],
    );
    let expanded_operation = operation(
        &expanded_workspace,
        &root.path().join("report.txt"),
        "approved",
    );
    assert_eq!(context.evaluate(source, &expanded_operation).decision, None);
    assert_eq!(
        context
            .evaluate(owner(Uuid::new_v4(), Uuid::new_v4()), &permitted)
            .decision,
        None
    );
}

#[test]
fn web_fetch_grants_cover_the_host_whatever_the_path_and_prompt() {
    let root = tempfile::tempdir().unwrap();
    let workspace = make_workspace(root.path(), Vec::new());
    let fetch = |url: &str, prompt: &str| {
        let prepared = tickloop_agent::tools::prepare(
            &workspace,
            "WebFetch",
            &json!({"url": url, "prompt": prompt}),
        )
        .unwrap();
        SessionPermissionOperation::from_prepared(&prepared, &workspace).unwrap()
    };
    let source = owner(Uuid::new_v4(), Uuid::new_v4());
    let mut context = SessionPermissionContext::default();
    context
        .record(
            source,
            fetch("https://docs.example.com/guide", "Summarize the guide"),
            SessionPermissionDecision::Allow,
        )
        .unwrap();

    for (url, prompt, decision) in [
        (
            "https://docs.example.com/api?page=2",
            "List the endpoints",
            Some(SessionPermissionDecision::Allow),
        ),
        (
            "http://docs.example.com/guide",
            "Summarize the guide",
            Some(SessionPermissionDecision::Allow),
        ),
        ("https://example.com/guide", "Summarize the guide", None),
        (
            "https://api.docs.example.com/guide",
            "Summarize the guide",
            None,
        ),
    ] {
        assert_eq!(
            context.evaluate(source, &fetch(url, prompt)).decision,
            decision,
            "{url}"
        );
    }
}

#[test]
fn frozen_denials_preserve_normalized_host_and_workspace_without_copying_allowances() {
    let root = tempfile::tempdir().unwrap();
    let other_root = tempfile::tempdir().unwrap();
    let workspace = make_workspace(root.path(), Vec::new());
    let source_chat = Uuid::new_v4();
    let source_loop = Uuid::new_v4();
    let source = owner(source_chat, source_loop);
    let fetch = |workspace: &Workspace, host: &str, path: &str| {
        let prepared = tickloop_agent::tools::prepare(
            workspace,
            "WebFetch",
            &json!({"url":format!("https://{host}/{path}"),"prompt":path}),
        )
        .unwrap();
        SessionPermissionOperation::from_prepared(&prepared, workspace).unwrap()
    };
    let mut context = SessionPermissionContext::default();
    context
        .record(
            source,
            fetch(&workspace, "blocked.example.com", "first"),
            SessionPermissionDecision::Deny,
        )
        .unwrap();
    context
        .record(
            source,
            fetch(&workspace, "allowed.example.com", "first"),
            SessionPermissionDecision::Allow,
        )
        .unwrap();
    let delegating_chat = Uuid::new_v4();
    let delegating_loop = Uuid::new_v4();
    context
        .register_branch(source_chat, Some(source_loop), delegating_chat)
        .unwrap();
    context
        .inherit_loop(delegating_chat, delegating_loop)
        .unwrap();
    let frozen = context.freeze_denials(owner(delegating_chat, delegating_loop));
    assert_eq!(frozen.len(), 1);
    let restored: Vec<session_permissions::SessionPermissionDenial> =
        serde_json::from_value(serde_json::to_value(frozen).unwrap()).unwrap();
    assert!(restored[0].matches(&fetch(&workspace, "blocked.example.com", "different")));
    assert!(!restored[0].matches(&fetch(&workspace, "allowed.example.com", "first")));
    assert!(!restored[0].matches(&fetch(
        &make_workspace(other_root.path(), vec![]),
        "blocked.example.com",
        "first"
    )));
}

#[test]
fn branch_inheritance_maps_each_child_loop_to_the_selected_source() {
    let root = tempfile::tempdir().unwrap();
    let workspace = make_workspace(root.path(), Vec::new());
    let source_chat = Uuid::new_v4();
    let source_loop = Uuid::new_v4();
    let child_chat = Uuid::new_v4();
    let child_loop = Uuid::new_v4();
    let unrelated_child_loop = Uuid::new_v4();
    let permitted = operation(&workspace, &root.path().join("report.txt"), "approved");
    let mut context = SessionPermissionContext::default();
    context
        .record(
            owner(source_chat, source_loop),
            permitted.clone(),
            SessionPermissionDecision::Allow,
        )
        .unwrap();

    context
        .register_branch(source_chat, Some(source_loop), child_chat)
        .unwrap();
    assert_eq!(
        context
            .evaluate(owner(child_chat, child_loop), &permitted)
            .decision,
        None
    );
    assert_eq!(
        context.inherit_loop(child_chat, child_loop).unwrap(),
        LoopInheritance::Inherited
    );
    assert_eq!(
        context
            .evaluate(owner(child_chat, child_loop), &permitted)
            .decision,
        Some(SessionPermissionDecision::Allow)
    );
    assert_eq!(
        context
            .evaluate(owner(child_chat, unrelated_child_loop), &permitted)
            .decision,
        None
    );
    let revision = context.revision();
    context
        .register_branch(source_chat, Some(source_loop), child_chat)
        .unwrap();
    assert_eq!(context.revision(), revision);
    assert_eq!(
        context.inherit_loop(child_chat, child_loop).unwrap(),
        LoopInheritance::Inherited
    );
    assert_eq!(context.revision(), revision);
    assert_eq!(
        context
            .inherit_loop(child_chat, unrelated_child_loop)
            .unwrap(),
        LoopInheritance::Inherited
    );
    assert_eq!(
        context
            .evaluate(owner(child_chat, unrelated_child_loop), &permitted)
            .decision,
        Some(SessionPermissionDecision::Allow)
    );
}

#[test]
fn absent_source_loop_never_widens_a_grant_to_the_child_chat() {
    let root = tempfile::tempdir().unwrap();
    let workspace = make_workspace(root.path(), Vec::new());
    let source_chat = Uuid::new_v4();
    let source_loop = Uuid::new_v4();
    let child_chat = Uuid::new_v4();
    let child_loop = Uuid::new_v4();
    let permitted = operation(&workspace, &root.path().join("report.txt"), "approved");
    let mut context = SessionPermissionContext::default();
    context
        .record(
            owner(source_chat, source_loop),
            permitted.clone(),
            SessionPermissionDecision::Allow,
        )
        .unwrap();
    context
        .register_branch(source_chat, None, child_chat)
        .unwrap();

    assert_eq!(
        context.inherit_loop(child_chat, child_loop).unwrap(),
        LoopInheritance::NoSourceLoop
    );
    assert_eq!(
        context
            .evaluate(owner(child_chat, child_loop), &permitted)
            .decision,
        None
    );
}

#[test]
fn branch_of_branch_inherits_only_explicit_owner_lineage() {
    let root = tempfile::tempdir().unwrap();
    let workspace = make_workspace(root.path(), Vec::new());
    let source_chat = Uuid::new_v4();
    let source_loop = Uuid::new_v4();
    let child_chat = Uuid::new_v4();
    let child_loop = Uuid::new_v4();
    let grandchild_chat = Uuid::new_v4();
    let grandchild_loop = Uuid::new_v4();
    let permitted = operation(&workspace, &root.path().join("report.txt"), "approved");
    let mut context = SessionPermissionContext::default();
    context
        .record(
            owner(source_chat, source_loop),
            permitted.clone(),
            SessionPermissionDecision::Allow,
        )
        .unwrap();
    context
        .register_branch(source_chat, Some(source_loop), child_chat)
        .unwrap();
    context.inherit_loop(child_chat, child_loop).unwrap();
    context
        .register_branch(child_chat, Some(child_loop), grandchild_chat)
        .unwrap();
    context
        .inherit_loop(grandchild_chat, grandchild_loop)
        .unwrap();

    assert_eq!(
        context
            .evaluate(owner(grandchild_chat, grandchild_loop), &permitted)
            .decision,
        Some(SessionPermissionDecision::Allow)
    );
}

#[test]
fn deny_precedence_and_revision_tokens_fail_closed() {
    let root = tempfile::tempdir().unwrap();
    let workspace = make_workspace(root.path(), Vec::new());
    let source = owner(Uuid::new_v4(), Uuid::new_v4());
    let permitted = operation(&workspace, &root.path().join("report.txt"), "approved");
    let unrelated = operation(&workspace, &root.path().join("other.txt"), "other");
    let mut context = SessionPermissionContext::default();
    context
        .record(source, permitted.clone(), SessionPermissionDecision::Allow)
        .unwrap();
    let allowed = context.evaluate(source, &permitted);

    context
        .record(source, unrelated, SessionPermissionDecision::Allow)
        .unwrap();
    assert!(!context.revalidate(&allowed.token, source, &permitted));

    context
        .record(source, permitted.clone(), SessionPermissionDecision::Deny)
        .unwrap();
    let denied = context.evaluate(source, &permitted);
    assert_eq!(denied.decision, Some(SessionPermissionDecision::Deny));
    assert!(context.revalidate(&denied.token, source, &permitted));
}

#[test]
fn branch_registration_is_idempotent_but_reparenting_and_cycles_are_rejected() {
    let root = Uuid::new_v4();
    let child = Uuid::new_v4();
    let grandchild = Uuid::new_v4();
    let mut context = SessionPermissionContext::default();

    context.register_branch(root, None, child).unwrap();
    let revision = context.revision();
    context.register_branch(root, None, child).unwrap();
    assert_eq!(context.revision(), revision);
    assert_eq!(
        context
            .register_branch(Uuid::new_v4(), None, child)
            .unwrap_err(),
        SessionPermissionError::ChildAlreadyRegistered
    );
    context.register_branch(child, None, grandchild).unwrap();
    assert_eq!(
        context.register_branch(grandchild, None, root).unwrap_err(),
        SessionPermissionError::BranchCycle
    );
    assert_eq!(
        context.register_branch(root, None, root).unwrap_err(),
        SessionPermissionError::SelfBranch
    );
    assert_eq!(
        context
            .inherit_loop(Uuid::new_v4(), Uuid::new_v4())
            .unwrap(),
        LoopInheritance::Unregistered
    );
}

#[test]
fn normalized_constructor_rejects_ambiguous_tools_and_canonicalizes_roots() {
    let root = tempfile::tempdir().unwrap();
    let additional = tempfile::tempdir().unwrap();
    let workspace = make_workspace(
        root.path(),
        vec![
            additional.path().to_path_buf(),
            additional.path().to_path_buf(),
            root.path().to_path_buf(),
        ],
    );
    let scope = WorkspaceScope::capture(&workspace).unwrap();
    let reordered_scope = WorkspaceScope::capture(&make_workspace(
        root.path(),
        vec![root.path().to_path_buf(), additional.path().to_path_buf()],
    ))
    .unwrap();
    assert_eq!(scope, reordered_scope);
    let operation = SessionPermissionOperation::from_normalized(
        "Read".into(),
        json!({"file_path": root.path().join("report.txt")}),
        scope.clone(),
    )
    .unwrap();
    let source = owner(Uuid::new_v4(), Uuid::new_v4());
    let mut context = SessionPermissionContext::default();
    context
        .record(source, operation.clone(), SessionPermissionDecision::Deny)
        .unwrap();
    assert_eq!(
        context.evaluate(source, &operation).decision,
        Some(SessionPermissionDecision::Deny)
    );
    assert_eq!(
        SessionPermissionOperation::from_normalized(" Read".into(), json!({}), scope).unwrap_err(),
        SessionPermissionError::InvalidTool
    );
    assert_eq!(
        PermissionOwner::new(Uuid::nil(), Uuid::new_v4()).unwrap_err(),
        SessionPermissionError::InvalidOwner
    );
    let file = root.path().join("not-a-directory");
    std::fs::write(&file, "data").unwrap();
    assert_eq!(
        WorkspaceScope::capture(&make_workspace(&file, Vec::new())).unwrap_err(),
        SessionPermissionError::InvalidWorkspace(file)
    );
}

#[test]
fn cloned_context_can_stage_branch_lineage_without_mutating_the_live_context() {
    let root = tempfile::tempdir().unwrap();
    let workspace = make_workspace(root.path(), Vec::new());
    let source_chat = Uuid::new_v4();
    let source_loop = Uuid::new_v4();
    let child_chat = Uuid::new_v4();
    let child_loop = Uuid::new_v4();
    let permitted = operation(&workspace, &root.path().join("report.txt"), "approved");
    let mut live = SessionPermissionContext::default();
    live.record(
        owner(source_chat, source_loop),
        permitted.clone(),
        SessionPermissionDecision::Allow,
    )
    .unwrap();
    let live_revision = live.revision();
    let mut candidate = live.clone();

    candidate
        .register_branch(source_chat, Some(source_loop), child_chat)
        .unwrap();
    candidate.inherit_loop(child_chat, child_loop).unwrap();

    assert_eq!(live.revision(), live_revision);
    assert_eq!(
        live.evaluate(owner(child_chat, child_loop), &permitted)
            .decision,
        None
    );
    assert_eq!(
        candidate
            .evaluate(owner(child_chat, child_loop), &permitted)
            .decision,
        Some(SessionPermissionDecision::Allow)
    );
}
