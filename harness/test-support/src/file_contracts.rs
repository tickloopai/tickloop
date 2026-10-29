//! Behavioral contracts for the file operations of Execution Environment Adapters. Each one only
//! uses the Port, so every environment runs the same cases; what the Port cannot do (seed files
//! with chosen modification times, lose a reply, hold a read) comes from a [`FileContractHost`].

use std::{
    collections::HashMap,
    path::Path,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, SystemTime},
};

use tickloop_harness_runtime::environment::{memory::MemoryEnvironment, *};
use tickloop_harness_tools::file_search::{glob_text, grep_text};
use tokio_util::sync::CancellationToken;

const READ_LIMIT: usize = 1 << 20;
const LIST_LIMIT: usize = 1000;
const HELD_READ_WAIT: Duration = Duration::from_millis(100);

/// An environment under test with an empty Folder that it lets the Port read and write.
pub trait FileContractHost {
    fn environment(&self) -> &dyn ExecutionEnvironment;
    /// The same scope on every call; its Folder root allows reading and writing.
    fn scope(&self) -> ExecutionScope;
    /// Stores a file below the Folder without the Port, as a change from outside the Agent.
    fn put_file(&self, path: &str, content: &[u8], modified: SystemTime);
    /// Makes the next file write take effect but lose its reply.
    fn lose_next_reply(&self);
    /// Makes reads of the Folder file `path` wait until the returned release runs.
    fn hold_reads(&self, path: &str) -> Box<dyn FnOnce() + Send>;
}

/// The in-memory environment with an empty Folder root `project` at `/project`.
pub struct MemoryFileHost {
    memory: MemoryEnvironment,
    scope: ExecutionScope,
}

impl MemoryFileHost {
    pub const FOLDER: &str = "project";
    pub const FOLDER_PATH: &str = "/project";

    /// A host whose scope also names these Additional Directories, to define with
    /// [`MemoryEnvironment::define_root`].
    pub fn with_additional_directories(additional_directories: &[&str]) -> Self {
        let memory = MemoryEnvironment::new();
        memory.define_root(Self::FOLDER, Self::FOLDER_PATH, FileAccess::ReadWrite);
        Self {
            memory,
            scope: ExecutionScope {
                chat: tickloop_ids::generate().into(),
                folder: Some(Self::FOLDER_PATH.into()),
                additional_directories: additional_directories
                    .iter()
                    .map(|directory| (*directory).into())
                    .collect(),
            },
        }
    }

    pub fn memory(&self) -> &MemoryEnvironment {
        &self.memory
    }
}

impl Default for MemoryFileHost {
    fn default() -> Self {
        Self::with_additional_directories(&[])
    }
}

impl FileContractHost for MemoryFileHost {
    fn environment(&self) -> &dyn ExecutionEnvironment {
        &self.memory
    }

    fn scope(&self) -> ExecutionScope {
        self.scope.clone()
    }

    fn put_file(&self, path: &str, content: &[u8], modified: SystemTime) {
        self.memory.put_file(Some(Self::FOLDER), path, content);
        self.memory.set_modified(Some(Self::FOLDER), path, modified);
    }

    fn lose_next_reply(&self) {
        self.memory.lose_next_reply();
    }

    fn hold_reads(&self, _: &str) -> Box<dyn FnOnce() + Send> {
        let release = self.memory.hold_file_reads();
        Box::new(move || release.cancel())
    }
}

/// A fresh temporary directory on this machine as the Folder of an environment that works on local
/// files, such as Code's Local Environment or [`FolderEnvironment`]. A lost reply and a held read
/// come from a wrapper around that environment's Ports.
pub struct LocalFileHost {
    environment: FaultyEnvironment,
    folder: tempfile::TempDir,
    scope: ExecutionScope,
}

impl LocalFileHost {
    pub fn new(environment: Arc<dyn ExecutionEnvironment>) -> Self {
        let folder = tempfile::tempdir().expect("a temporary Folder");
        let scope = ExecutionScope {
            chat: tickloop_ids::generate().into(),
            folder: Some(folder.path().to_string_lossy().into_owned()),
            additional_directories: Vec::new(),
        };
        Self {
            environment: FaultyEnvironment {
                inner: environment,
                faults: Arc::default(),
            },
            folder,
            scope,
        }
    }

    /// The Folder as the scope names it, which need not be its real path.
    pub fn folder(&self) -> &Path {
        self.folder.path()
    }
}

impl FileContractHost for LocalFileHost {
    fn environment(&self) -> &dyn ExecutionEnvironment {
        &self.environment
    }

    fn scope(&self) -> ExecutionScope {
        self.scope.clone()
    }

    fn put_file(&self, path: &str, content: &[u8], modified: SystemTime) {
        let path = self.folder.path().join(path);
        std::fs::create_dir_all(path.parent().expect("a file below the Folder"))
            .expect("the file's directories");
        std::fs::write(&path, content).expect("the file");
        std::fs::File::options()
            .write(true)
            .open(&path)
            .and_then(|file| file.set_modified(modified))
            .expect("the file's modification time");
    }

    fn lose_next_reply(&self) {
        self.environment
            .faults
            .lose_next_reply
            .store(true, Ordering::SeqCst);
    }

    fn hold_reads(&self, path: &str) -> Box<dyn FnOnce() + Send> {
        let release = CancellationToken::new();
        self.environment
            .faults
            .held_reads
            .lock()
            .unwrap()
            .insert(path.into(), release.clone());
        Box::new(move || release.cancel())
    }
}

#[derive(Default)]
struct Faults {
    lose_next_reply: AtomicBool,
    held_reads: Mutex<HashMap<String, CancellationToken>>,
}

/// Binds Ports that pass everything on to the wrapped environment's, except the faults asked for.
struct FaultyEnvironment {
    inner: Arc<dyn ExecutionEnvironment>,
    faults: Arc<Faults>,
}

impl ExecutionEnvironment for FaultyEnvironment {
    fn bind(
        &self,
        scope: ExecutionScope,
        authority: ExecutionAuthority,
    ) -> Result<Arc<dyn ExecutionPort>, ExecutionError> {
        Ok(Arc::new(FaultyPort {
            inner: self.inner.bind(scope, authority)?,
            faults: self.faults.clone(),
        }))
    }
}

struct FaultyPort {
    inner: Arc<dyn ExecutionPort>,
    faults: Arc<Faults>,
}

#[async_trait::async_trait]
impl ExecutionPort for FaultyPort {
    async fn inspect(&self, query: ExecutionQuery) -> Result<ExecutionInspection, ExecutionError> {
        if let ExecutionQuery::File(FileQuery::Read { path, .. }) = &query {
            let held = self
                .faults
                .held_reads
                .lock()
                .unwrap()
                .get(path.path())
                .cloned();
            if let Some(release) = held {
                release.cancelled().await;
            }
        }
        self.inner.inspect(query).await
    }

    async fn prepare(&self, intent: ExecutionIntent) -> Result<PreparedExecution, ExecutionError> {
        self.inner.prepare(intent).await
    }

    async fn execute(
        &self,
        prepared: PreparedExecution,
        admission: ExecutionAdmission,
        cancel: CancellationToken,
    ) -> ExecutionOutcome {
        let operation = prepared.operation();
        let is_file_change = prepared.facts().intent.file_path().is_some();
        let outcome = self.inner.execute(prepared, admission, cancel).await;
        if is_file_change && self.faults.lose_next_reply.swap(false, Ordering::SeqCst) {
            return ExecutionOutcome::OutcomeUnknown {
                operation,
                reason: "the reply was lost".into(),
            };
        }
        outcome
    }

    async fn observe(
        &self,
        request: ObserveRequest,
    ) -> Result<ExecutionObservation, ExecutionError> {
        self.inner.observe(request).await
    }
}

/// Every environment-agnostic file case, each on a fresh host.
pub async fn file_contract<H: FileContractHost>(fresh: impl Fn() -> H) {
    round_trip_contract(&fresh()).await;
    conflict_contract(&fresh()).await;
    outside_root_contract(&fresh()).await;
    lost_reply_contract(&fresh()).await;
    resolve_contract(&fresh()).await;
    held_read_contract(&fresh()).await;
    revocation_contract(&fresh()).await;
    glob_contract(&fresh()).await;
    grep_files_contract(&fresh()).await;
    grep_content_contract(&fresh()).await;
    grep_count_contract(&fresh()).await;
    grep_filter_contract(&fresh()).await;
    grep_paging_contract(&fresh()).await;
    grep_limits_contract(&fresh()).await;
}

/// A Port bound to `scope` with a root whose capability is `Read` refuses changes there at
/// `prepare`, before any approval, and a root whose capability is `None` refuses reads.
pub async fn read_only_root_contract(
    environment: &dyn ExecutionEnvironment,
    scope: ExecutionScope,
) {
    let authority = ExecutionAuthority::new(scope.chat);
    let port = environment.bind(scope, authority).unwrap();
    let ExecutionInspection::Capabilities(capabilities) =
        port.inspect(ExecutionQuery::Capabilities).await.unwrap()
    else {
        panic!("missing capabilities");
    };
    let read_only = capabilities
        .files
        .roots()
        .iter()
        .filter(|root| root.access == FileAccess::Read)
        .collect::<Vec<_>>();
    assert!(!read_only.is_empty(), "the scope has no read-only root");
    for root in read_only {
        let path = FilePath::in_root(root.name.clone(), "new.txt").unwrap();
        assert!(matches!(
            inspect_file(
                &port,
                FileQuery::Stat(FilePath::in_root(root.name.clone(), "").unwrap())
            )
            .await,
            Ok(FileInspection::Stat(FileStat::Directory { .. }))
        ));
        for intent in [
            ExecutionIntent::WriteFile {
                path: path.clone(),
                content: b"refused".to_vec(),
                expected: None,
            },
            ExecutionIntent::DeleteFile {
                path: path.clone(),
                expected: FileVersion::new("any"),
            },
        ] {
            let error = port.prepare(intent).await.err().expect("a refused change");
            assert_eq!(error.code, error_code::ACCESS_DENIED);
        }
        assert_code(
            inspect_file(&port, FileQuery::Stat(path)).await,
            error_code::NOT_FOUND,
        );
    }
    for root in capabilities
        .files
        .roots()
        .iter()
        .filter(|root| root.access == FileAccess::None)
    {
        let path = FilePath::in_root(root.name.clone(), "").unwrap();
        assert_code(
            inspect_file(&port, FileQuery::Stat(path)).await,
            error_code::ACCESS_DENIED,
        );
    }
}

struct Bound {
    port: Arc<dyn ExecutionPort>,
    authority: ExecutionAuthority,
    folder: RootName,
    files: FileCapabilities,
}

async fn bind(host: &impl FileContractHost) -> Bound {
    let scope = host.scope();
    let authority = ExecutionAuthority::new(scope.chat);
    let port = host.environment().bind(scope, authority.clone()).unwrap();
    let ExecutionInspection::Capabilities(capabilities) =
        port.inspect(ExecutionQuery::Capabilities).await.unwrap()
    else {
        panic!("missing capabilities");
    };
    let folder = capabilities.files.folder().expect("a Folder root");
    assert!(folder.access.can_write(), "the Folder must be writable");
    Bound {
        port,
        authority,
        folder: folder.name.clone(),
        files: capabilities.files.clone(),
    }
}

async fn inspect_file(
    port: &Arc<dyn ExecutionPort>,
    query: FileQuery,
) -> Result<FileInspection, ExecutionError> {
    match port.inspect(ExecutionQuery::File(query)).await? {
        ExecutionInspection::File(inspection) => Ok(inspection),
        other => panic!("not a file inspection: {other:?}"),
    }
}

#[track_caller]
fn assert_code<T: std::fmt::Debug>(result: Result<T, ExecutionError>, code: &str) {
    match result {
        Err(error) => assert_eq!(error.code, code, "{error}"),
        Ok(value) => panic!("expected {code}, got {value:?}"),
    }
}

impl Bound {
    fn path(&self, path: &str) -> FilePath {
        FilePath::in_root(self.folder.clone(), path).unwrap()
    }

    async fn inspect(&self, query: FileQuery) -> Result<FileInspection, ExecutionError> {
        inspect_file(&self.port, query).await
    }

    async fn stat(&self, path: &str) -> Result<FileStat, ExecutionError> {
        match self.inspect(FileQuery::Stat(self.path(path))).await? {
            FileInspection::Stat(stat) => Ok(stat),
            other => panic!("not a stat: {other:?}"),
        }
    }

    async fn read_range(
        &self,
        path: &str,
        offset: u64,
        limit: usize,
    ) -> Result<FileContent, ExecutionError> {
        match self
            .inspect(FileQuery::Read {
                path: self.path(path),
                offset,
                limit,
            })
            .await?
        {
            FileInspection::Read(content) => Ok(content),
            other => panic!("not a read: {other:?}"),
        }
    }

    async fn read(&self, path: &str) -> Result<FileContent, ExecutionError> {
        self.read_range(path, 0, READ_LIMIT).await
    }

    async fn list(&self, path: &str) -> Vec<(String, FileKind)> {
        match self
            .inspect(FileQuery::List {
                path: self.path(path),
                limit: LIST_LIMIT,
            })
            .await
            .unwrap()
        {
            FileInspection::List(listing) => {
                assert!(!listing.truncated);
                listing
                    .entries
                    .into_iter()
                    .map(|entry| (entry.name, entry.kind))
                    .collect()
            }
            other => panic!("not a listing: {other:?}"),
        }
    }

    /// The change's result, or why it failed before or at dispatch.
    async fn change(&self, intent: ExecutionIntent) -> Result<ExecutionResult, ExecutionError> {
        let prepared = self.port.prepare(intent).await?;
        let admission = self.authority.admit(&prepared)?;
        match self
            .port
            .execute(prepared, admission, CancellationToken::new())
            .await
        {
            ExecutionOutcome::Completed(result) => Ok(result),
            ExecutionOutcome::NotDispatched(error) => Err(error),
            ExecutionOutcome::OutcomeUnknown { reason, .. } => {
                panic!("unexpected unknown outcome: {reason}")
            }
        }
    }

    async fn write(
        &self,
        path: &str,
        content: &[u8],
        expected: Option<&FileMetadata>,
    ) -> Result<FileMetadata, ExecutionError> {
        let result = self
            .change(ExecutionIntent::WriteFile {
                path: self.path(path),
                content: content.to_vec(),
                expected: expected.map(|metadata| metadata.version.clone()),
            })
            .await?;
        Ok(*result.file.expect("a write reports the file it left"))
    }

    async fn glob(&self, base: &str, pattern: &str) -> GlobMatches {
        match self
            .inspect(FileQuery::Glob {
                path: self.path(base),
                query: GlobQuery {
                    pattern: pattern.into(),
                    limit: GLOB_RESULT_LIMIT,
                },
            })
            .await
            .unwrap()
        {
            FileInspection::Glob(matches) => matches,
            other => panic!("not a glob: {other:?}"),
        }
    }

    async fn grep_matches(
        &self,
        base: &str,
        query: &GrepQuery,
    ) -> Result<GrepMatches, ExecutionError> {
        match self
            .inspect(FileQuery::Grep {
                path: self.path(base),
                query: query.clone(),
            })
            .await?
        {
            FileInspection::Grep(matches) => Ok(matches),
            other => panic!("not a grep: {other:?}"),
        }
    }

    /// The text the `Grep` tool shows for this search in the Folder.
    async fn grep(&self, base: &str, query: GrepQuery) -> String {
        let matches = self.grep_matches(base, &query).await.unwrap();
        grep_text(&matches, &query, str::to_owned)
    }
}

fn metadata(stat: FileStat) -> FileMetadata {
    match stat {
        FileStat::File(metadata) => metadata,
        other => panic!("not a file: {other:?}"),
    }
}

fn grep(pattern: &str) -> GrepQuery {
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

fn content(pattern: &str) -> GrepQuery {
    GrepQuery {
        output_mode: GrepOutputMode::Content,
        ..grep(pattern)
    }
}

/// An hour ago plus `minutes`, so files can be ordered by modification time.
fn minute(minutes: u64) -> SystemTime {
    SystemTime::now() - Duration::from_secs(3600) + Duration::from_secs(60 * minutes)
}

/// Seeds files the way the local search tests write them: in order, the later ones newer.
fn seed(host: &impl FileContractHost, files: &[(&str, &str)]) {
    for (index, (path, content)) in files.iter().enumerate() {
        host.put_file(path, content.as_bytes(), minute(index as u64));
    }
}

async fn round_trip_contract(host: &impl FileContractHost) {
    let bound = bind(host).await;
    assert_code(bound.stat("notes/today.md").await, error_code::NOT_FOUND);
    assert_code(bound.read("notes/today.md").await, error_code::NOT_FOUND);

    let first = bound
        .write("notes/today.md", b"first draft", None)
        .await
        .unwrap();
    assert_eq!(first.len, 11);
    assert_eq!(metadata(bound.stat("notes/today.md").await.unwrap()), first);
    assert!(matches!(
        bound.stat("notes").await.unwrap(),
        FileStat::Directory { .. }
    ));
    let read = bound.read("notes/today.md").await.unwrap();
    assert_eq!(read.data, b"first draft");
    assert_eq!(read.metadata, first);
    assert_eq!(
        bound.read_range("notes/today.md", 6, 3).await.unwrap().data,
        b"dra"
    );
    assert!(
        bound
            .read_range("notes/today.md", 100, 3)
            .await
            .unwrap()
            .data
            .is_empty()
    );
    assert_code(bound.read("notes").await, error_code::IS_DIRECTORY);
    assert_eq!(
        bound.list("").await,
        [("notes".to_owned(), FileKind::Directory)]
    );
    assert_eq!(
        bound.list("notes").await,
        [("today.md".to_owned(), FileKind::File)]
    );

    let second = bound
        .write("notes/today.md", b"second draft", Some(&first))
        .await
        .unwrap();
    assert_ne!(second.version, first.version);
    assert_eq!(
        bound.read("notes/today.md").await.unwrap().data,
        b"second draft"
    );

    bound
        .change(ExecutionIntent::DeleteFile {
            path: bound.path("notes/today.md"),
            expected: second.version,
        })
        .await
        .unwrap();
    assert_code(bound.stat("notes/today.md").await, error_code::NOT_FOUND);
}

async fn conflict_contract(host: &impl FileContractHost) {
    let bound = bind(host).await;
    let first = bound.write("plan.md", b"ours", None).await.unwrap();

    assert_code(
        bound.write("plan.md", b"blind", None).await,
        error_code::CONFLICT,
    );
    assert_eq!(bound.read("plan.md").await.unwrap().data, b"ours");

    host.put_file("plan.md", b"theirs", SystemTime::now());
    assert_code(
        bound.write("plan.md", b"stale", Some(&first)).await,
        error_code::CONFLICT,
    );
    assert_code(
        bound
            .change(ExecutionIntent::DeleteFile {
                path: bound.path("plan.md"),
                expected: first.version.clone(),
            })
            .await,
        error_code::CONFLICT,
    );
    assert_eq!(bound.read("plan.md").await.unwrap().data, b"theirs");
}

async fn outside_root_contract(host: &impl FileContractHost) {
    let bound = bind(host).await;
    assert_eq!(
        FilePath::in_root(bound.folder.clone(), "../escape.txt")
            .unwrap_err()
            .code,
        error_code::OUTSIDE_ROOT
    );
    let unbound = RootName::new("not-a-root-of-this-port").unwrap();
    assert!(bound.files.root(&unbound).is_none());
    let elsewhere = FilePath::in_root(unbound, "file.txt").unwrap();
    assert_code(
        bound.inspect(FileQuery::Stat(elsewhere.clone())).await,
        error_code::OUTSIDE_ROOT,
    );
    assert_code(
        bound
            .change(ExecutionIntent::WriteFile {
                path: elsewhere,
                content: b"escape".to_vec(),
                expected: None,
            })
            .await,
        error_code::OUTSIDE_ROOT,
    );
    if bound.files.outside_roots() == FileAccess::None {
        let outside = FilePath::outside_roots("/etc/hosts").unwrap();
        assert_code(
            bound.inspect(FileQuery::Stat(outside.clone())).await,
            error_code::OUTSIDE_ROOT,
        );
        assert_code(
            bound
                .change(ExecutionIntent::WriteFile {
                    path: outside,
                    content: b"escape".to_vec(),
                    expected: None,
                })
                .await,
            error_code::OUTSIDE_ROOT,
        );
    }
}

async fn lost_reply_contract(host: &impl FileContractHost) {
    let bound = bind(host).await;
    let before = bound.write("lost.txt", b"before", None).await.unwrap();
    host.lose_next_reply();
    let prepared = bound
        .port
        .prepare(ExecutionIntent::WriteFile {
            path: bound.path("lost.txt"),
            content: b"after".to_vec(),
            expected: Some(before.version.clone()),
        })
        .await
        .unwrap();
    let operation = prepared.operation();
    let admission = bound.authority.admit(&prepared).unwrap();
    let outcome = bound
        .port
        .execute(prepared, admission, CancellationToken::new())
        .await;
    assert!(
        matches!(outcome, ExecutionOutcome::OutcomeUnknown { operation: lost, .. } if lost == operation),
        "{outcome:?}"
    );

    let read = bound.read("lost.txt").await.unwrap();
    match read.data.as_slice() {
        b"before" => assert_eq!(read.metadata.version, before.version),
        b"after" => {
            assert_ne!(read.metadata.version, before.version);
            assert_code(
                bound.write("lost.txt", b"after", Some(&before)).await,
                error_code::CONFLICT,
            );
        }
        other => panic!(
            "a lost write left neither the old nor the new content: {:?}",
            String::from_utf8_lossy(other)
        ),
    }
    let again = bound.read("lost.txt").await.unwrap();
    assert_eq!(again, read, "nothing sent the lost write again");
}

/// Paths without links resolve to themselves in their root, with what is there; a missing file
/// keeps its names, and the Folder's display path names the Folder root.
async fn resolve_contract(host: &impl FileContractHost) {
    let bound = bind(host).await;
    host.put_file("docs/guide.md", b"guide", SystemTime::now());
    let resolve = |path: FilePath| {
        let bound = &bound;
        async move {
            match bound.inspect(FileQuery::Resolve(path)).await.unwrap() {
                FileInspection::Resolved(resolved) => resolved,
                other => panic!("not a resolution: {other:?}"),
            }
        }
    };
    for (path, kind) in [
        ("docs/guide.md", Some(FileKind::File)),
        ("docs", Some(FileKind::Directory)),
        ("", Some(FileKind::Directory)),
        ("docs/missing/new.md", None),
    ] {
        assert_eq!(
            resolve(bound.path(path)).await,
            ResolvedPath {
                path: bound.path(path),
                kind
            },
            "{path:?}"
        );
    }
    let folder = bound.files.folder().expect("a Folder root");
    let shown = bound
        .files
        .resolve(&format!("{}/docs/guide.md", folder.display_path))
        .unwrap();
    assert_eq!(shown, bound.path("docs/guide.md"));
    assert_eq!(resolve(shown).await.path, bound.path("docs/guide.md"));
}

async fn held_read_contract(host: &impl FileContractHost) {
    let bound = bind(host).await;
    host.put_file("held.txt", b"held", SystemTime::now());
    host.put_file("other.txt", b"other", SystemTime::now());
    let release = host.hold_reads("held.txt");
    assert!(
        tokio::time::timeout(HELD_READ_WAIT, bound.read("held.txt"))
            .await
            .is_err(),
        "the held read finished"
    );
    release();

    assert_eq!(bound.read("other.txt").await.unwrap().data, b"other");
    let written = bound.write("after.txt", b"after", None).await.unwrap();
    let read = bound.read("after.txt").await.unwrap();
    assert_eq!(read.data, b"after");
    assert_eq!(read.metadata, written);
}

async fn revocation_contract(host: &impl FileContractHost) {
    let bound = bind(host).await;
    let written = bound.write("kept.txt", b"kept", None).await.unwrap();
    let prepared = bound
        .port
        .prepare(ExecutionIntent::WriteFile {
            path: bound.path("kept.txt"),
            content: b"too late".to_vec(),
            expected: Some(written.version.clone()),
        })
        .await
        .unwrap();
    let admission = bound.authority.admit(&prepared).unwrap();
    bound.authority.revoke();

    assert!(matches!(
        bound.port.execute(prepared, admission, CancellationToken::new()).await,
        ExecutionOutcome::NotDispatched(error) if error.code == error_code::AUTHORITY_REVOKED
    ));
    assert_code(bound.stat("kept.txt").await, error_code::AUTHORITY_REVOKED);
    assert_code(bound.read("kept.txt").await, error_code::AUTHORITY_REVOKED);
    assert_code(
        bound
            .inspect(FileQuery::List {
                path: bound.path(""),
                limit: LIST_LIMIT,
            })
            .await,
        error_code::AUTHORITY_REVOKED,
    );
    assert_code(
        bound
            .inspect(FileQuery::Glob {
                path: bound.path(""),
                query: GlobQuery {
                    pattern: "*".into(),
                    limit: GLOB_RESULT_LIMIT,
                },
            })
            .await,
        error_code::AUTHORITY_REVOKED,
    );
    assert_code(
        bound.grep_matches("", &grep("kept")).await,
        error_code::AUTHORITY_REVOKED,
    );
    for intent in [
        ExecutionIntent::WriteFile {
            path: bound.path("new.txt"),
            content: b"new".to_vec(),
            expected: None,
        },
        ExecutionIntent::DeleteFile {
            path: bound.path("kept.txt"),
            expected: written.version.clone(),
        },
    ] {
        let error = bound.port.prepare(intent).await.err().expect("refused");
        assert_eq!(error.code, error_code::AUTHORITY_REVOKED);
    }

    let next = bind(host).await;
    assert_eq!(next.read("kept.txt").await.unwrap().data, b"kept");
}

/// `glob_lists_ignored_and_hidden_files_oldest_first_relative_to_the_workdir` and
/// `glob_accepts_an_absolute_pattern_and_keeps_the_first_hundred_files` in `search_tests.rs`.
async fn glob_contract(host: &impl FileContractHost) {
    let bound = bind(host).await;
    seed(
        host,
        &[
            (".git/HEAD", "ref: refs/heads/main\n"),
            (".gitignore", "ignored*\n"),
            (".git/hooks/check.rs", ""),
            (".tickloop/state.rs", ""),
            ("service-secret.rs", ""),
            ("notes.txt", ""),
            ("lib.rs", ""),
            (".config/hidden.rs", ""),
            ("ignored.rs", ""),
            ("src/main.rs", ""),
        ],
    );
    let text = |matches: GlobMatches| glob_text(&matches, str::to_owned);
    assert_eq!(
        text(bound.glob("", "**/*.rs").await),
        "lib.rs\n.config/hidden.rs\nignored.rs\nsrc/main.rs"
    );
    assert_eq!(text(bound.glob("src", "*.rs").await), "src/main.rs");
    assert_eq!(text(bound.glob("", "**/*.md").await), "No files found");

    for index in (0..101).rev() {
        host.put_file(
            &format!("reports/{index:03}.txt"),
            b"",
            minute(20 + index as u64),
        );
    }
    let matches = bound.glob("reports", "*.txt").await;
    assert!(matches.truncated);
    assert_eq!(matches.files.len(), GLOB_RESULT_LIMIT);
    assert_eq!(matches.files[0].path, "reports/000.txt");
    assert_eq!(matches.files[99].path, "reports/099.txt");
    assert!(
        matches
            .files
            .windows(2)
            .all(|pair| pair[0].modified <= pair[1].modified)
    );
    let listed = text(matches);
    assert_eq!(listed.lines().count(), 101);
    assert_eq!(
        listed.lines().last(),
        Some("(Results are truncated. Consider using a more specific path or pattern.)")
    );
}

/// `grep_lists_matching_files_newest_first_honoring_gitignore_and_skipping_vcs`.
async fn grep_files_contract(host: &impl FileContractHost) {
    let bound = bind(host).await;
    seed(
        host,
        &[
            (".git/HEAD", "ref: refs/heads/main\n"),
            (".gitignore", "ignored*\n"),
            ("ignored.md", "delivery\n"),
            (".git/COMMIT_EDITMSG", "delivery\n"),
            (".hg/store.md", "delivery\n"),
            (".jj/repo.md", "delivery\n"),
            (".tickloop/chat.md", "delivery\n"),
            ("delivery-token.md", "delivery\n"),
            ("unrelated.md", "nothing\n"),
            ("docs/plan.md", "Delivery on Friday\n"),
            (".notes/today.md", "delivery confirmed\n"),
            ("readme.md", "Next delivery\n"),
        ],
    );
    assert_eq!(
        bound.grep("", grep("elivery")).await,
        "Found 3 files\nreadme.md\n.notes/today.md\ndocs/plan.md"
    );
    assert_eq!(
        bound
            .grep(
                "",
                GrepQuery {
                    case_insensitive: true,
                    globs: vec!["docs/*.md".into()],
                    ..grep("Delivery")
                }
            )
            .await,
        "Found 1 file\ndocs/plan.md"
    );
    assert_eq!(bound.grep("", grep("absent")).await, "No files found");
}

/// `grep_content_numbers_lines_and_shows_requested_context`.
async fn grep_content_contract(host: &impl FileContractHost) {
    let bound = bind(host).await;
    seed(
        host,
        &[
            (
                "log.txt",
                "one\ntwo\nmatch A\nthree\nfour\nfive\nsix\nmatch B\nseven\n",
            ),
            ("other.txt", "no\nmatch C\n"),
        ],
    );
    let around = |before, after| GrepQuery {
        lines_before: before,
        lines_after: after,
        ..content("match")
    };
    assert_eq!(
        bound.grep("", content("match")).await,
        "log.txt:3:match A\nlog.txt:8:match B\nother.txt:2:match C"
    );
    assert_eq!(
        bound
            .grep(
                "log.txt",
                GrepQuery {
                    line_numbers: false,
                    ..content("match")
                }
            )
            .await,
        "match A\nmatch B"
    );
    assert_eq!(
        bound.grep("log.txt", around(1, 1)).await,
        "2-two\n3:match A\n4-three\n--\n7-six\n8:match B\n9-seven"
    );
    assert_eq!(
        bound
            .grep(
                "",
                GrepQuery {
                    globs: vec!["log.txt".into()],
                    ..around(1, 1)
                }
            )
            .await,
        "log.txt-2-two\nlog.txt:3:match A\nlog.txt-4-three\n--\nlog.txt-7-six\nlog.txt:8:match B\nlog.txt-9-seven"
    );
    assert_eq!(
        bound.grep("log.txt", around(2, 2)).await,
        "1-one\n2-two\n3:match A\n4-three\n5-four\n6-five\n7-six\n8:match B\n9-seven"
    );
    assert_eq!(
        bound.grep("", around(0, 1)).await,
        "log.txt:3:match A\nlog.txt-4-three\n--\nlog.txt:8:match B\nlog.txt-9-seven\n--\nother.txt:2:match C"
    );
    assert_eq!(bound.grep("", content("absent")).await, "No matches found");
}

/// `grep_counts_matching_lines_per_file_with_a_total`.
async fn grep_count_contract(host: &impl FileContractHost) {
    let bound = bind(host).await;
    seed(
        host,
        &[
            ("a.txt", "delivery\ndelivery delivery\nnone\n"),
            ("b.txt", "delivery\n"),
            ("c.txt", "none\n"),
        ],
    );
    let count = |pattern| GrepQuery {
        output_mode: GrepOutputMode::Count,
        ..grep(pattern)
    };
    assert_eq!(
        bound.grep("", count("delivery")).await,
        "a.txt:2\nb.txt:1\n\nFound 3 total occurrences across 2 files."
    );
    assert_eq!(
        bound.grep("b.txt", count("delivery")).await,
        "b.txt:1\n\nFound 1 total occurrence across 1 file."
    );
    assert_eq!(
        bound.grep("", count("absent")).await,
        "No matches found\n\nFound 0 total occurrences across 0 files."
    );
}

/// `grep_filters_by_file_type_case_and_multiline_patterns`.
async fn grep_filter_contract(host: &impl FileContractHost) {
    let bound = bind(host).await;
    seed(
        host,
        &[
            ("view.tsx", "ORDER\n"),
            ("script.py", "class Order:\n    id = 1\n"),
            ("main.rs", "struct Order {\n    id: u32,\n}\n"),
        ],
    );
    assert_eq!(
        bound
            .grep(
                "",
                GrepQuery {
                    file_type: Some("rust".into()),
                    ..grep("Order")
                }
            )
            .await,
        "Found 1 file\nmain.rs"
    );
    assert_eq!(
        bound
            .grep(
                "",
                GrepQuery {
                    case_insensitive: true,
                    globs: vec!["*.{py,tsx}".into()],
                    ..grep("order")
                }
            )
            .await,
        "Found 2 files\nscript.py\nview.tsx"
    );
    assert_eq!(
        bound
            .grep(
                "",
                GrepQuery {
                    case_insensitive: true,
                    globs: vec!["*.rs".into(), "*.py".into()],
                    ..grep("order")
                }
            )
            .await,
        "Found 2 files\nmain.rs\nscript.py"
    );
    assert_eq!(
        bound
            .grep(
                "",
                GrepQuery {
                    multiline: true,
                    ..content("Order \\{.*id")
                }
            )
            .await,
        "main.rs:1:struct Order {\nmain.rs:2:    id: u32,"
    );
    assert_eq!(
        bound.grep("", grep("Order \\{.*id")).await,
        "No files found"
    );
    for invalid in [
        GrepQuery {
            file_type: Some("cobol-ish".into()),
            ..grep("Order")
        },
        grep("(unclosed"),
    ] {
        assert_code(
            bound.grep_matches("", &invalid).await,
            error_code::INVALID_INPUT,
        );
    }
}

/// `grep_pages_results_with_head_limit_and_offset`.
async fn grep_paging_contract(host: &impl FileContractHost) {
    let bound = bind(host).await;
    let lines = (1..=300)
        .map(|index| format!("item {index}"))
        .collect::<Vec<_>>();
    host.put_file("items.txt", (lines.join("\n") + "\n").as_bytes(), minute(0));
    for index in 0..260 {
        host.put_file(
            &format!("many/{index:03}.txt"),
            b"item\n",
            minute(1 + index as u64),
        );
    }
    let page = |head_limit, offset| GrepQuery {
        head_limit,
        offset,
        ..content("item")
    };

    let default = bound.grep("items.txt", page(None, 0)).await;
    let (listed, note) = default.split_once("\n\n").unwrap();
    assert_eq!(listed.lines().count(), 250);
    assert!(listed.ends_with("250:item 250"));
    assert_eq!(note, "[Showing results with pagination = limit: 250]");
    let unlimited = bound.grep("items.txt", page(Some(0), 0)).await;
    assert_eq!(unlimited.lines().count(), 300);
    assert!(!unlimited.contains("pagination"));
    assert_eq!(
        bound.grep("items.txt", page(Some(2), 10)).await,
        "11:item 11\n12:item 12\n\n[Showing results with pagination = limit: 2, offset: 10]"
    );
    assert_eq!(
        bound.grep("items.txt", page(None, 298)).await,
        "299:item 299\n300:item 300\n\n[Showing results with pagination = offset: 298]"
    );

    let files_page = bound.grep("many", grep("item")).await;
    let mut lines = files_page.lines();
    assert_eq!(lines.next(), Some("Found 250 files limit: 250"));
    assert_eq!(lines.next(), Some("many/259.txt"));
    assert_eq!(lines.count(), 249);
    assert_eq!(
        bound
            .grep(
                "many",
                GrepQuery {
                    head_limit: Some(1),
                    offset: 259,
                    ..grep("item")
                }
            )
            .await,
        "Found 1 file offset: 259\nmany/000.txt"
    );
    assert_eq!(
        bound
            .grep(
                "many",
                GrepQuery {
                    output_mode: GrepOutputMode::Count,
                    head_limit: Some(2),
                    ..grep("item")
                }
            )
            .await,
        "many/000.txt:1\nmany/001.txt:1\n\nFound 2 total occurrences across 2 files. with pagination = limit: 2"
    );
}

/// `grep_omits_long_lines_and_stops_at_binary_data` and `grep_skips_files_above_eight_mebibytes`.
async fn grep_limits_contract(host: &impl FileContractHost) {
    let bound = bind(host).await;
    host.put_file(
        "minified.js",
        format!("token {}\nshort token\n", "x".repeat(600)).as_bytes(),
        minute(0),
    );
    host.put_file("image.bin", b"\0\0token\n", minute(1));
    host.put_file(
        "large.log",
        format!("token\n{}", "x".repeat(8 * 1024 * 1024)).as_bytes(),
        minute(2),
    );
    host.put_file("small.log", b"token\n", minute(3));

    assert_eq!(
        bound.grep("minified.js", content("token")).await,
        "1:[Omitted long matching line]\n2:short token"
    );
    assert_eq!(
        bound
            .grep(
                "",
                GrepQuery {
                    globs: vec!["*.js".into(), "*.bin".into()],
                    ..grep("token")
                }
            )
            .await,
        "Found 1 file\nminified.js"
    );
    assert_eq!(
        bound
            .grep(
                "",
                GrepQuery {
                    globs: vec!["*.log".into()],
                    ..grep("token")
                }
            )
            .await,
        "Found 1 file\nsmall.log"
    );
    assert_code(
        bound.grep_matches("large.log", &grep("token")).await,
        error_code::TOO_LARGE,
    );
}
