use std::{
    os::unix::fs::{PermissionsExt, symlink},
    path::Path,
    sync::Arc,
};

use tickloop_code_local_environment::{
    environment::LocalEnvironment,
    hosted::{
        shells::OwnTickhost,
        tools::HostedTickTools,
        worker_control::{DaemonControl, WorkerControlGrant},
    },
};
use tickloop_harness::runtime::environment::{
    ExecutionAuthority, ExecutionCapabilities, ExecutionEnvironment, ExecutionError,
    ExecutionInspection, ExecutionIntent, ExecutionOutcome, ExecutionPort, ExecutionQuery,
    ExecutionScope, FileAccess, FileInspection, FileKind, FileMetadata, FilePath, FileQuery,
    FileStat, FolderEnvironment, GLOB_RESULT_LIMIT, GlobQuery, GrepOutput, GrepOutputMode,
    GrepQuery, ResolvedPath, RootName, error_code,
};
use tickloop_harness::test_support::file_contracts::{
    LocalFileHost, MemoryFileHost, file_contract,
};
use tokio_util::sync::CancellationToken;

/// The private permissions a new file gets.
const NEW_FILE_MODE: u32 = 0o600;
const READ_LIMIT: usize = 1 << 20;

#[tokio::test]
async fn local_memory_and_folder_environments_share_the_file_contract() {
    file_contract(MemoryFileHost::default).await;
    file_contract(|| LocalFileHost::new(Arc::new(LocalEnvironment::bare()))).await;
    file_contract(|| LocalFileHost::new(Arc::new(FolderEnvironment::bare()))).await;
}

fn hosted_ticks(directory: &Path) -> Arc<HostedTickTools> {
    Arc::new(HostedTickTools::new(
        DaemonControl::new(WorkerControlGrant {
            endpoint: directory.join("absent.sock"),
            host_endpoint: directory.join("host.sock"),
            daemon_epoch: tickloop_ids::generate(),
            binding: tickloop_ids::generate(),
            capability: "test-only".into(),
        }),
        Vec::new(),
    ))
}

async fn capabilities(environment: &dyn ExecutionEnvironment, scope: ExecutionScope) -> Bound {
    let authority = ExecutionAuthority::new(scope.chat);
    let port = environment.bind(scope, authority.clone()).unwrap();
    let ExecutionInspection::Capabilities(capabilities) =
        port.inspect(ExecutionQuery::Capabilities).await.unwrap()
    else {
        panic!("missing capabilities");
    };
    Bound {
        port,
        authority,
        capabilities,
    }
}

fn scope(folder: Option<&Path>, additional_directories: &[&Path]) -> ExecutionScope {
    ExecutionScope {
        chat: tickloop_ids::generate().into(),
        folder: folder.map(|folder| folder.to_string_lossy().into_owned()),
        additional_directories: additional_directories
            .iter()
            .map(|directory| directory.to_string_lossy().into_owned())
            .collect(),
    }
}

fn real(path: &Path) -> String {
    path.canonicalize().unwrap().to_string_lossy().into_owned()
}

#[tokio::test]
async fn every_local_environment_offers_the_folder_and_additional_directories_by_real_path() {
    let directory = tempfile::tempdir().unwrap();
    let folder = directory.path().join("project");
    let other = directory.path().join("other/project");
    let missing = directory.path().join("later");
    std::fs::create_dir_all(&folder).unwrap();
    std::fs::create_dir_all(&other).unwrap();
    std::fs::write(other.join("guide.md"), "read me").unwrap();
    let environments: [(&str, LocalEnvironment, bool, bool); 4] = [
        ("bare", LocalEnvironment::bare(), false, false),
        (
            "with_shells",
            LocalEnvironment::with_shells(Arc::new(OwnTickhost::new())),
            false,
            true,
        ),
        (
            "with_ticks",
            LocalEnvironment::with_ticks(hosted_ticks(directory.path())),
            true,
            false,
        ),
        (
            "with",
            LocalEnvironment::with(Arc::new(OwnTickhost::new()), hosted_ticks(directory.path())),
            true,
            true,
        ),
    ];
    for (name, environment, ticks, shells) in environments {
        let bound = capabilities(
            &environment,
            scope(Some(&folder), &[&other, &folder, &missing]),
        )
        .await;
        assert_eq!(
            (bound.capabilities.ticks, bound.capabilities.shells),
            (ticks, shells),
            "{name}"
        );
        let roots = bound
            .capabilities
            .files
            .roots()
            .iter()
            .map(|root| {
                (
                    root.name.as_str().to_owned(),
                    root.display_path.clone(),
                    root.access,
                    root.is_folder,
                    root.local_path.clone(),
                )
            })
            .collect::<Vec<_>>();
        let later = format!("{}/later", real(directory.path()));
        assert_eq!(
            roots,
            [
                (
                    "project".into(),
                    real(&folder),
                    FileAccess::ReadWrite,
                    true,
                    Some(folder.canonicalize().unwrap())
                ),
                (
                    "project-2".into(),
                    real(&other),
                    FileAccess::ReadWrite,
                    false,
                    Some(other.canonicalize().unwrap())
                ),
                (
                    "later".into(),
                    later.clone(),
                    FileAccess::ReadWrite,
                    false,
                    Some(later.into())
                ),
            ],
            "{name}"
        );
        assert_eq!(
            bound.capabilities.files.outside_roots(),
            FileAccess::ReadWrite
        );
        let guide = bound
            .capabilities
            .files
            .resolve(&format!("{}/guide.md", real(&other)))
            .unwrap();
        assert_eq!(guide.root().map(RootName::as_str), Some("project-2"));
        assert_eq!(bound.read(&guide).await.unwrap(), b"read me");

        let folderless = capabilities(&environment, scope(None, &[&other])).await;
        assert!(folderless.capabilities.files.roots().is_empty(), "{name}");
        assert_eq!(
            folderless.capabilities.files.outside_roots(),
            FileAccess::None
        );
    }
}

struct Bound {
    port: Arc<dyn ExecutionPort>,
    authority: ExecutionAuthority,
    capabilities: ExecutionCapabilities,
}

impl Bound {
    async fn local(folder: &Path) -> Self {
        capabilities(&LocalEnvironment::bare(), scope(Some(folder), &[])).await
    }

    fn path(&self, below: &str) -> FilePath {
        let folder = self.capabilities.files.folder().unwrap();
        FilePath::in_root(folder.name.clone(), below).unwrap()
    }

    async fn inspect(&self, query: FileQuery) -> Result<FileInspection, ExecutionError> {
        match self.port.inspect(ExecutionQuery::File(query)).await? {
            ExecutionInspection::File(inspection) => Ok(inspection),
            other => panic!("not a file inspection: {other:?}"),
        }
    }

    async fn read(&self, path: &FilePath) -> Result<Vec<u8>, ExecutionError> {
        match self
            .inspect(FileQuery::Read {
                path: path.clone(),
                offset: 0,
                limit: READ_LIMIT,
            })
            .await?
        {
            FileInspection::Read(content) => Ok(content.data),
            other => panic!("not a read: {other:?}"),
        }
    }

    async fn metadata(&self, below: &str) -> FileMetadata {
        match self.inspect(FileQuery::Stat(self.path(below))).await {
            Ok(FileInspection::Stat(FileStat::File(metadata))) => metadata,
            other => panic!("not a file: {other:?}"),
        }
    }

    async fn write(
        &self,
        below: &str,
        content: &[u8],
        expected: Option<&FileMetadata>,
    ) -> Result<FileMetadata, ExecutionError> {
        let prepared = self
            .port
            .prepare(ExecutionIntent::WriteFile {
                path: self.path(below),
                content: content.to_vec(),
                expected: expected.map(|metadata| metadata.version.clone()),
            })
            .await?;
        self.execute(prepared)
            .await
            .map(|file| file.expect("a write reports its file"))
    }

    async fn execute(
        &self,
        prepared: tickloop_harness::runtime::environment::PreparedExecution,
    ) -> Result<Option<FileMetadata>, ExecutionError> {
        let admission = self.authority.admit(&prepared).unwrap();
        match self
            .port
            .execute(prepared, admission, CancellationToken::new())
            .await
        {
            ExecutionOutcome::Completed(result) => Ok(result.file.map(|file| *file)),
            ExecutionOutcome::NotDispatched(error) => Err(error),
            ExecutionOutcome::OutcomeUnknown { reason, .. } => panic!("unknown outcome: {reason}"),
        }
    }
}

#[track_caller]
fn assert_code<T: std::fmt::Debug>(result: Result<T, ExecutionError>, code: &str) {
    match result {
        Err(error) => assert_eq!(error.code, code, "{error}"),
        Ok(value) => panic!("expected {code}, got {value:?}"),
    }
}

#[tokio::test]
async fn symbolic_links_out_of_a_root_are_refused_for_reads_and_writes() {
    let folder = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    std::fs::write(outside.path().join("secret.txt"), "outside").unwrap();
    std::fs::write(folder.path().join("notes.txt"), "inside").unwrap();
    symlink(
        outside.path().join("secret.txt"),
        folder.path().join("escape"),
    )
    .unwrap();
    symlink(outside.path(), folder.path().join("escape-directory")).unwrap();
    symlink(
        outside.path().join("missing.txt"),
        folder.path().join("dangling"),
    )
    .unwrap();
    symlink(folder.path().join("notes.txt"), folder.path().join("alias")).unwrap();
    let bound = Bound::local(folder.path()).await;

    assert_eq!(bound.read(&bound.path("alias")).await.unwrap(), b"inside");
    for below in ["escape", "escape-directory/secret.txt"] {
        assert_code(
            bound.inspect(FileQuery::Stat(bound.path(below))).await,
            error_code::OUTSIDE_ROOT,
        );
        assert_code(
            bound.read(&bound.path(below)).await,
            error_code::OUTSIDE_ROOT,
        );
    }
    for (below, query) in [
        (
            "escape-directory",
            FileQuery::Glob {
                path: bound.path("escape-directory"),
                query: GlobQuery {
                    pattern: "*".into(),
                    limit: GLOB_RESULT_LIMIT,
                },
            },
        ),
        (
            "escape",
            FileQuery::Grep {
                path: bound.path("escape"),
                query: files_with("outside"),
            },
        ),
    ] {
        assert_code(bound.inspect(query).await, error_code::OUTSIDE_ROOT);
        assert_code(
            bound
                .inspect(FileQuery::List {
                    path: bound.path(below),
                    limit: 10,
                })
                .await,
            error_code::OUTSIDE_ROOT,
        );
    }
    for below in [
        "escape",
        "escape-directory/secret.txt",
        "escape-directory/new.txt",
        "dangling",
    ] {
        assert_code(
            bound.write(below, b"escaped", None).await,
            error_code::OUTSIDE_ROOT,
        );
    }
    assert_eq!(
        std::fs::read(outside.path().join("secret.txt")).unwrap(),
        b"outside"
    );
    assert!(!outside.path().join("new.txt").exists());
    assert!(!outside.path().join("missing.txt").exists());
}

#[tokio::test]
async fn resolving_a_path_names_where_its_links_lead() {
    let folder = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    std::fs::write(folder.path().join("notes.txt"), "inside").unwrap();
    std::fs::write(outside.path().join("secret.txt"), "outside").unwrap();
    symlink(folder.path().join("notes.txt"), folder.path().join("alias")).unwrap();
    symlink(
        outside.path().join("secret.txt"),
        folder.path().join("escape"),
    )
    .unwrap();
    symlink(
        outside.path().join("missing.txt"),
        folder.path().join("dangling"),
    )
    .unwrap();
    let bound = Bound::local(folder.path()).await;
    let resolve = |path: FilePath| {
        let bound = &bound;
        async move {
            match bound.inspect(FileQuery::Resolve(path)).await? {
                FileInspection::Resolved(resolved) => Ok(resolved),
                other => panic!("not a resolution: {other:?}"),
            }
        }
    };
    let real_outside = outside.path().canonicalize().unwrap();
    for (below, expected, kind) in [
        ("alias", bound.path("notes.txt"), Some(FileKind::File)),
        (
            "escape",
            FilePath::outside_roots(real_outside.join("secret.txt").to_str().unwrap()).unwrap(),
            Some(FileKind::File),
        ),
        ("new/plan.md", bound.path("new/plan.md"), None),
    ] {
        assert_eq!(
            resolve(bound.path(below)).await.unwrap(),
            ResolvedPath {
                path: expected,
                kind
            },
            "{below}"
        );
    }
    assert_code(
        resolve(bound.path("dangling")).await,
        error_code::OUTSIDE_ROOT,
    );
    let spelled =
        FilePath::outside_roots(folder.path().join("notes.txt").to_str().unwrap()).unwrap();
    assert_eq!(
        resolve(spelled).await.unwrap().path,
        bound.path("notes.txt"),
        "a path spelled through a link to the root lands in the root"
    );
}

#[tokio::test]
async fn a_target_replaced_after_approval_is_a_conflict_and_nothing_changes() {
    let folder = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let bound = Bound::local(folder.path()).await;
    let root = folder.path();
    std::fs::write(outside.path().join("target.txt"), "outside").unwrap();

    type Replacement<'a> = Box<dyn Fn(&Path) + 'a>;
    let replacements: [(&str, Replacement); 3] = [
        (
            "plan.md",
            Box::new(|root: &Path| {
                std::fs::rename(root.join("plan.md"), root.join("plan.old")).unwrap();
                symlink(outside.path().join("target.txt"), root.join("plan.md")).unwrap();
            }),
        ),
        (
            "notes.md",
            Box::new(|root: &Path| {
                std::fs::write(root.join("staged"), "replacement").unwrap();
                std::fs::rename(root.join("staged"), root.join("notes.md")).unwrap();
            }),
        ),
        (
            "nested/draft.md",
            Box::new(|root: &Path| {
                std::fs::rename(root.join("nested"), root.join("nested.old")).unwrap();
                symlink(outside.path(), root.join("nested")).unwrap();
            }),
        ),
    ];
    for (below, replace) in replacements {
        let original = bound.write(below, b"original", None).await.unwrap();
        let prepared = bound
            .port
            .prepare(ExecutionIntent::WriteFile {
                path: bound.path(below),
                content: b"approved".to_vec(),
                expected: Some(original.version.clone()),
            })
            .await
            .unwrap();
        replace(root);
        assert_code(bound.execute(prepared).await, error_code::CONFLICT);
    }
    assert_eq!(std::fs::read(root.join("plan.old")).unwrap(), b"original");
    assert_eq!(
        std::fs::read(outside.path().join("target.txt")).unwrap(),
        b"outside"
    );
    assert_eq!(
        std::fs::read(root.join("notes.md")).unwrap(),
        b"replacement"
    );
    assert_eq!(
        std::fs::read(root.join("nested.old/draft.md")).unwrap(),
        b"original"
    );
    assert!(!outside.path().join("draft.md").exists());

    let kept = bound.write("kept.md", b"kept", None).await.unwrap();
    let delete = bound
        .port
        .prepare(ExecutionIntent::DeleteFile {
            path: bound.path("kept.md"),
            expected: kept.version,
        })
        .await
        .unwrap();
    std::fs::write(root.join("kept.md"), "changed meanwhile").unwrap();
    assert_code(bound.execute(delete).await, error_code::CONFLICT);
    assert_eq!(
        std::fs::read(root.join("kept.md")).unwrap(),
        b"changed meanwhile"
    );
}

#[tokio::test]
async fn new_files_get_missing_parents_and_overwrites_keep_permission_bits() {
    let folder = tempfile::tempdir().unwrap();
    let bound = Bound::local(folder.path()).await;
    let root = folder.path();

    let created = bound
        .write("reports/2026/october.md", b"draft", None)
        .await
        .unwrap();
    assert_eq!(
        std::fs::read(root.join("reports/2026/october.md")).unwrap(),
        b"draft"
    );
    let mode = |path: &Path| std::fs::metadata(path).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode(&root.join("reports/2026/october.md")), NEW_FILE_MODE);
    assert_eq!(bound.metadata("reports/2026/october.md").await, created);

    std::fs::write(root.join("run.sh"), "#!/bin/sh\n").unwrap();
    std::fs::set_permissions(root.join("run.sh"), std::fs::Permissions::from_mode(0o751)).unwrap();
    let before = bound.metadata("run.sh").await;
    let after = bound
        .write("run.sh", b"#!/bin/sh\nexit 0\n", Some(&before))
        .await
        .unwrap();
    assert_ne!(after.version, before.version);
    assert_eq!(mode(&root.join("run.sh")), 0o751);
    assert_eq!(
        std::fs::read(root.join("run.sh")).unwrap(),
        b"#!/bin/sh\nexit 0\n"
    );
    assert_eq!(
        std::fs::read_dir(root)
            .unwrap()
            .filter_map(|entry| entry.ok())
            .filter(|entry| entry
                .file_name()
                .to_string_lossy()
                .starts_with(".tickloop-write-"))
            .count(),
        0,
        "no staged file is left behind"
    );
}

fn files_with(pattern: &str) -> GrepQuery {
    GrepQuery {
        pattern: pattern.into(),
        globs: Vec::new(),
        file_type: None,
        output_mode: GrepOutputMode::FilesWithMatches,
        lines_before: 0,
        lines_after: 0,
        line_numbers: true,
        case_insensitive: false,
        multiline: false,
        head_limit: None,
        offset: 0,
    }
}

/// `searching_does_not_follow_symbolic_links` and the outside-workspace search of
/// `searches_outside_the_workspace_need_approval_and_inside_run_without_it` in `search_tests.rs`.
#[tokio::test]
async fn local_searches_skip_symbolic_links_and_show_paths_outside_roots_below_slash() {
    let folder = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let root = folder.path();
    std::fs::write(outside.path().join("linked.txt"), "delivery\n").unwrap();
    std::fs::create_dir(outside.path().join("nested")).unwrap();
    std::fs::write(outside.path().join("nested/inner.txt"), "delivery\n").unwrap();
    symlink(outside.path().join("linked.txt"), root.join("file.txt")).unwrap();
    symlink(outside.path().join("nested"), root.join("nested")).unwrap();
    std::fs::write(root.join("local.txt"), "delivery\n").unwrap();
    let bound = Bound::local(root).await;
    let glob = |path: FilePath| FileQuery::Glob {
        path,
        query: GlobQuery {
            pattern: "**/*.txt".into(),
            limit: GLOB_RESULT_LIMIT,
        },
    };
    let found = |inspection: FileInspection| match inspection {
        FileInspection::Glob(matches) => matches
            .files
            .into_iter()
            .map(|file| file.path)
            .collect::<Vec<_>>(),
        FileInspection::Grep(matches) => match matches.output {
            GrepOutput::Files(files) => files.into_iter().map(|file| file.path).collect(),
            other => panic!("not files: {other:?}"),
        },
        other => panic!("not a search: {other:?}"),
    };

    assert_eq!(
        found(bound.inspect(glob(bound.path(""))).await.unwrap()),
        ["local.txt"]
    );
    assert_eq!(
        found(
            bound
                .inspect(FileQuery::Grep {
                    path: bound.path(""),
                    query: files_with("delivery"),
                })
                .await
                .unwrap()
        ),
        ["local.txt"]
    );

    let elsewhere = FilePath::outside_roots(&real(outside.path())).unwrap();
    let below_slash = |name: &str| {
        format!("{}/{name}", real(outside.path()))
            .trim_start_matches('/')
            .to_owned()
    };
    assert_eq!(
        found(bound.inspect(glob(elsewhere.clone())).await.unwrap()),
        [below_slash("linked.txt"), below_slash("nested/inner.txt")]
    );
    assert_eq!(
        found(
            bound
                .inspect(FileQuery::Grep {
                    path: elsewhere,
                    query: files_with("delivery"),
                })
                .await
                .unwrap()
        )
        .len(),
        2
    );
}
