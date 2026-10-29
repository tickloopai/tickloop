use tickloop_harness_runtime::environment::{
    ExecutionAuthority, ExecutionEnvironment, ExecutionInspection, ExecutionIntent,
    ExecutionOutcome, ExecutionQuery, ExecutionScope, FileAccess, FilePath, FileQuery,
    FolderEnvironment, RootName, error_code,
};
use tickloop_harness_test_support::file_contracts::{
    FileContractHost, MemoryFileHost, file_contract, read_only_root_contract,
};
use tokio_util::sync::CancellationToken;

const FOLDER: &str = MemoryFileHost::FOLDER;

fn root(name: &str) -> RootName {
    RootName::new(name).unwrap()
}

#[tokio::test]
async fn memory_environment_meets_the_file_contract() {
    file_contract(MemoryFileHost::default).await;
}

#[tokio::test]
async fn read_only_and_closed_roots_refuse_what_their_capability_lacks() {
    let host = MemoryFileHost::with_additional_directories(&["/reference", "/sealed"]);
    host.memory()
        .define_root("reference", "/reference", FileAccess::Read);
    host.memory()
        .define_root("sealed", "/sealed", FileAccess::None);
    host.memory()
        .put_file(Some("reference"), "guide.md", "read me");
    read_only_root_contract(host.environment(), host.scope()).await;
}

#[tokio::test]
async fn ports_offer_the_roots_their_scope_names() {
    let host = MemoryFileHost::with_additional_directories(&["/docs"]);
    let environment = host.memory();
    environment.define_root("docs", "/docs", FileAccess::Read);
    environment.define_root("unlisted", "/unlisted", FileAccess::ReadWrite);
    environment.set_outside_root_access(FileAccess::Read);
    let scope = host.scope();
    let port = environment
        .bind(scope.clone(), ExecutionAuthority::new(scope.chat))
        .unwrap();
    let ExecutionInspection::Capabilities(capabilities) =
        port.inspect(ExecutionQuery::Capabilities).await.unwrap()
    else {
        panic!("missing capabilities");
    };
    let roots = capabilities
        .files
        .roots()
        .iter()
        .map(|root| {
            (
                root.name.as_str(),
                root.display_path.as_str(),
                root.access,
                root.is_folder,
                root.local_path.is_none(),
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(
        roots,
        [
            ("docs", "/docs", FileAccess::Read, false, true),
            (
                FOLDER,
                MemoryFileHost::FOLDER_PATH,
                FileAccess::ReadWrite,
                true,
                true
            ),
        ]
    );
    assert_eq!(capabilities.files.outside_roots(), FileAccess::Read);
    assert_eq!(
        capabilities.files.resolve("/docs/a.md").unwrap(),
        FilePath::in_root(root("docs"), "a.md").unwrap()
    );
    let unlisted = FilePath::in_root(root("unlisted"), "").unwrap();
    let error = port
        .inspect(ExecutionQuery::File(FileQuery::Stat(unlisted)))
        .await
        .unwrap_err();
    assert_eq!(error.code, error_code::OUTSIDE_ROOT);
}

#[tokio::test]
async fn a_cancelled_write_held_after_dispatch_is_outcome_unknown_and_took_effect() {
    let host = MemoryFileHost::default();
    let scope = host.scope();
    let authority = ExecutionAuthority::new(scope.chat);
    let port = host.environment().bind(scope, authority.clone()).unwrap();
    let release = host.memory().hold_call_replies();
    let prepared = port
        .prepare(ExecutionIntent::WriteFile {
            path: FilePath::in_root(root(FOLDER), "held.txt").unwrap(),
            content: b"written".to_vec(),
            expected: None,
        })
        .await
        .unwrap();
    let operation = prepared.operation();
    let admission = authority.admit(&prepared).unwrap();
    let cancel = CancellationToken::new();
    let pending = tokio::spawn({
        let port = port.clone();
        let cancel = cancel.clone();
        async move { port.execute(prepared, admission, cancel).await }
    });
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while host.memory().file(Some(FOLDER), "held.txt").is_none() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the write was dispatched");
    cancel.cancel();
    let outcome = pending.await.unwrap();
    assert!(
        matches!(outcome, ExecutionOutcome::OutcomeUnknown { operation: id, .. } if id == operation),
        "{outcome:?}"
    );
    assert_eq!(
        host.memory().file(Some(FOLDER), "held.txt").as_deref(),
        Some(&b"written"[..])
    );
    assert!(matches!(
        port.inspect(ExecutionQuery::Receipt(operation))
            .await
            .unwrap(),
        ExecutionInspection::Receipt(_)
    ));
    release.cancel();
}

#[tokio::test]
async fn a_folder_environment_without_a_folder_offers_no_files() {
    let directory = tempfile::tempdir().unwrap();
    let scope = ExecutionScope {
        chat: tickloop_ids::generate().into(),
        folder: None,
        additional_directories: vec![directory.path().to_string_lossy().into_owned()],
    };
    let port = FolderEnvironment::bare()
        .bind(scope.clone(), ExecutionAuthority::new(scope.chat))
        .unwrap();
    let ExecutionInspection::Capabilities(capabilities) =
        port.inspect(ExecutionQuery::Capabilities).await.unwrap()
    else {
        panic!("missing capabilities");
    };
    assert!(capabilities.files.roots().is_empty());
    assert_eq!(capabilities.files.outside_roots(), FileAccess::None);
    let error = port
        .inspect(ExecutionQuery::File(FileQuery::Stat(
            FilePath::outside_roots("/").unwrap(),
        )))
        .await
        .unwrap_err();
    assert_eq!(error.code, error_code::OUTSIDE_ROOT);
}
