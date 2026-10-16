use std::{
    collections::HashMap,
    ffi::{OsStr, OsString},
    fs::OpenOptions,
    io::Read,
    os::unix::{
        ffi::{OsStrExt, OsStringExt},
        fs::OpenOptionsExt,
    },
    path::{Component, Path, PathBuf},
    process::{Command, Output, Stdio},
};

use tickloop_agent_protocol::{
    ChatSnapshot, Content, FileChangeAction, FileChangeReviewSummary, FileIdentity,
    OperationRecord, OperationState, Role, RunId,
};
use tickloop_agent_tools::file_change::{
    FileChangeArtifactError, FileChangeReview, build_file_change_review, load_file_change_review,
};

const MAX_CURRENT_TEXT_BYTES: u64 = 8 * 1024 * 1024;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ChangeCatalog {
    pub sources: Vec<ChangeSource>,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub enum ChangeSourceId {
    Current,
    AgentRun(RunId),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ChangeSourceState {
    Ready,
    Clean,
    NonGit,
    GitError { message: String },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ChangeSource {
    pub id: ChangeSourceId,
    pub state: ChangeSourceState,
    pub user_prompt: Option<String>,
    pub files: Vec<CatalogFileChange>,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub enum CatalogFileChangeId {
    CurrentPath(PathBuf),
    CurrentIndexPath(PathBuf),
    CurrentWorktreePath(PathBuf),
    AgentOperation(uuid::Uuid),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CatalogFileChange {
    pub id: CatalogFileChangeId,
    pub path: PathBuf,
    pub action: FileChangeAction,
    pub current_status: Option<CurrentFileStatus>,
    pub summary: Option<FileChangeReviewSummary>,
    pub detail: CatalogChangeDetail,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CurrentFileStatus {
    pub index: GitFileState,
    pub worktree: GitFileState,
    pub untracked: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GitFileState {
    Unmodified,
    Modified,
    Added,
    Deleted,
    TypeChanged,
    Unmerged,
    Unknown(char),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CatalogChangeDetail {
    Available(Box<FileChangeReview>),
    Unavailable {
        kind: CatalogUnavailableKind,
        message: String,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CatalogUnavailableKind {
    MissingArtifact,
    CorruptArtifact,
    ArtifactIo,
    UnsupportedText,
    ReadError,
    GitError,
}

impl ChangeCatalog {
    pub async fn load(snapshot: &ChatSnapshot) -> Self {
        let owned_snapshot = snapshot.clone();
        let fallback_snapshot = snapshot.clone();
        match tokio::task::spawn_blocking(move || build_catalog(&owned_snapshot)).await {
            Ok(catalog) => catalog,
            Err(error) => Self {
                sources: std::iter::once(ChangeSource {
                    id: ChangeSourceId::Current,
                    state: ChangeSourceState::GitError {
                        message: format!("change catalog task did not finish: {error}"),
                    },
                    user_prompt: None,
                    files: Vec::new(),
                })
                .chain(agent_sources(&fallback_snapshot))
                .collect(),
            },
        }
    }
}

fn build_catalog(snapshot: &ChatSnapshot) -> ChangeCatalog {
    let mut sources = vec![current_source(&snapshot.workdir)];
    sources.extend(agent_sources(snapshot));
    ChangeCatalog { sources }
}

fn agent_sources(snapshot: &ChatSnapshot) -> Vec<ChangeSource> {
    let mut sources = Vec::<ChangeSource>::new();
    let mut source_indices = HashMap::<RunId, usize>::new();
    for operation in snapshot.operations.iter().filter(|operation| {
        operation.state == OperationState::Completed && operation.file_change.is_some()
    }) {
        let source_index = *source_indices.entry(operation.run_id).or_insert_with(|| {
            let index = sources.len();
            sources.push(ChangeSource {
                id: ChangeSourceId::AgentRun(operation.run_id),
                state: ChangeSourceState::Ready,
                user_prompt: user_prompt(snapshot, operation.run_id),
                files: Vec::new(),
            });
            index
        });
        sources[source_index]
            .files
            .push(agent_file_change(operation));
    }
    sources
}

fn agent_file_change(operation: &OperationRecord) -> CatalogFileChange {
    let prepared = operation
        .file_change
        .as_ref()
        .expect("completed file operation filter guarantees a review reference");
    let detail = match load_file_change_review(&prepared.artifact) {
        Ok(review) if review.summary() == prepared.summary => {
            CatalogChangeDetail::Available(Box::new(review))
        }
        Ok(_) => unavailable(
            CatalogUnavailableKind::CorruptArtifact,
            "file change review summary does not match its artifact",
        ),
        Err(error) => artifact_unavailable(error),
    };
    CatalogFileChange {
        id: CatalogFileChangeId::AgentOperation(operation.id),
        path: prepared.summary.path.clone(),
        action: prepared.summary.action,
        current_status: None,
        summary: Some(prepared.summary.clone()),
        detail,
    }
}

fn user_prompt(snapshot: &ChatSnapshot, run_id: RunId) -> Option<String> {
    snapshot
        .messages
        .iter()
        .enumerate()
        .find(|(index, message)| {
            message.role == Role::User
                && snapshot.message_runs.get(index) == Some(&run_id)
                && !snapshot.host_notes.contains_key(index)
        })
        .map(|(_, message)| {
            message
                .content
                .iter()
                .filter_map(|content| match content {
                    Content::Text { text } => Some(text.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n")
        })
        .filter(|prompt| !prompt.is_empty())
}

fn artifact_unavailable(error: FileChangeArtifactError) -> CatalogChangeDetail {
    let kind = match &error {
        FileChangeArtifactError::Missing(_) => CatalogUnavailableKind::MissingArtifact,
        FileChangeArtifactError::Corrupt { .. } => CatalogUnavailableKind::CorruptArtifact,
        FileChangeArtifactError::Io { .. } => CatalogUnavailableKind::ArtifactIo,
    };
    unavailable(kind, error.to_string())
}

fn unavailable(kind: CatalogUnavailableKind, message: impl Into<String>) -> CatalogChangeDetail {
    CatalogChangeDetail::Unavailable {
        kind,
        message: message.into(),
    }
}

fn current_source(workdir: &Path) -> ChangeSource {
    let repository = match discover_repository(workdir) {
        Ok(repository) => repository,
        Err(state) => {
            return ChangeSource {
                id: ChangeSourceId::Current,
                state,
                user_prompt: None,
                files: Vec::new(),
            };
        }
    };
    let statuses = match read_statuses(&repository.root) {
        Ok(statuses) => statuses,
        Err(message) => {
            return ChangeSource {
                id: ChangeSourceId::Current,
                state: ChangeSourceState::GitError { message },
                user_prompt: None,
                files: Vec::new(),
            };
        }
    };
    if statuses.is_empty() {
        return ChangeSource {
            id: ChangeSourceId::Current,
            state: ChangeSourceState::Clean,
            user_prompt: None,
            files: Vec::new(),
        };
    }
    let files = statuses
        .into_iter()
        .flat_map(|(path, status)| current_file_changes(&repository, path, status))
        .collect();
    ChangeSource {
        id: ChangeSourceId::Current,
        state: ChangeSourceState::Ready,
        user_prompt: None,
        files,
    }
}

struct Repository {
    root: PathBuf,
    has_head: bool,
}

fn discover_repository(workdir: &Path) -> Result<Repository, ChangeSourceState> {
    let output = run_git(
        workdir,
        [OsStr::new("rev-parse"), OsStr::new("--show-toplevel")],
    )
    .map_err(|message| ChangeSourceState::GitError { message })?;
    if !output.status.success() {
        let message = git_error(&output);
        return Err(if has_git_marker(workdir) {
            ChangeSourceState::GitError { message }
        } else {
            ChangeSourceState::NonGit
        });
    }
    let mut root = output.stdout;
    if root.last() == Some(&b'\n') {
        root.pop();
    }
    if root.is_empty() {
        return Err(ChangeSourceState::GitError {
            message: "git returned an empty repository root".into(),
        });
    }
    let root = PathBuf::from(OsString::from_vec(root));
    let head = run_git(
        &root,
        [
            OsStr::new("rev-parse"),
            OsStr::new("--verify"),
            OsStr::new("--quiet"),
            OsStr::new("HEAD"),
        ],
    )
    .map_err(|message| ChangeSourceState::GitError { message })?;
    if !head.status.success() && !head.stderr.is_empty() {
        return Err(ChangeSourceState::GitError {
            message: git_error(&head),
        });
    }
    Ok(Repository {
        root,
        has_head: head.status.success(),
    })
}

fn has_git_marker(path: &Path) -> bool {
    path.ancestors()
        .any(|ancestor| ancestor.join(".git").exists())
}

fn read_statuses(root: &Path) -> Result<Vec<(PathBuf, CurrentFileStatus)>, String> {
    let output = run_git(
        root,
        [
            OsStr::new("status"),
            OsStr::new("--porcelain=v1"),
            OsStr::new("-z"),
            OsStr::new("--untracked-files=all"),
            OsStr::new("--no-renames"),
            OsStr::new("--ignore-submodules=none"),
        ],
    )?;
    if !output.status.success() {
        return Err(git_error(&output));
    }
    let mut statuses = std::collections::BTreeMap::<PathBuf, CurrentFileStatus>::new();
    for record in output
        .stdout
        .split(|byte| *byte == 0)
        .filter(|record| !record.is_empty())
    {
        if record.len() < 4 || record[2] != b' ' {
            return Err("git returned malformed porcelain status".into());
        }
        let path = PathBuf::from(OsString::from_vec(record[3..].to_vec()));
        let status = parse_status(record[0], record[1]);
        statuses
            .entry(path)
            .and_modify(|existing| merge_status(existing, status))
            .or_insert(status);
    }
    Ok(statuses.into_iter().collect())
}

fn parse_status(index: u8, worktree: u8) -> CurrentFileStatus {
    let untracked = index == b'?' && worktree == b'?';
    CurrentFileStatus {
        index: if untracked {
            GitFileState::Unmodified
        } else {
            git_file_state(index)
        },
        worktree: if untracked {
            GitFileState::Unmodified
        } else {
            git_file_state(worktree)
        },
        untracked,
    }
}

fn git_file_state(status: u8) -> GitFileState {
    match status {
        b' ' => GitFileState::Unmodified,
        b'M' => GitFileState::Modified,
        b'A' => GitFileState::Added,
        b'D' => GitFileState::Deleted,
        b'T' => GitFileState::TypeChanged,
        b'U' => GitFileState::Unmerged,
        other => GitFileState::Unknown(other as char),
    }
}

fn merge_status(existing: &mut CurrentFileStatus, additional: CurrentFileStatus) {
    if existing.index == GitFileState::Unmodified {
        existing.index = additional.index;
    }
    if existing.worktree == GitFileState::Unmodified {
        existing.worktree = additional.worktree;
    }
    existing.untracked |= additional.untracked;
}

fn current_file_changes(
    repository: &Repository,
    relative_path: PathBuf,
    status: CurrentFileStatus,
) -> Vec<CatalogFileChange> {
    if !is_repository_relative_path(&relative_path) {
        return vec![CatalogFileChange {
            id: CatalogFileChangeId::CurrentWorktreePath(relative_path.clone()),
            path: relative_path,
            action: FileChangeAction::Overwrite,
            current_status: Some(status),
            summary: None,
            detail: unavailable(
                CatalogUnavailableKind::GitError,
                "git returned a path outside the repository",
            ),
        }];
    }

    if status.untracked {
        let baseline = TextSnapshot::missing();
        let worktree = read_worktree_text(&repository.root.join(&relative_path));
        return vec![current_file_change(
            CatalogFileChangeId::CurrentWorktreePath(relative_path.clone()),
            relative_path,
            CurrentFileStatus {
                index: GitFileState::Unmodified,
                worktree: GitFileState::Unmodified,
                untracked: true,
            },
            baseline,
            worktree,
        )];
    }

    let mut changes = Vec::with_capacity(2);
    let index = read_index_text(repository, &relative_path, status);
    if status.index != GitFileState::Unmodified {
        let head = read_head_text(repository, &relative_path, status);
        changes.push(current_file_change(
            CatalogFileChangeId::CurrentIndexPath(relative_path.clone()),
            relative_path.clone(),
            CurrentFileStatus {
                index: status.index,
                worktree: GitFileState::Unmodified,
                untracked: false,
            },
            head,
            index.clone(),
        ));
    }
    if status.worktree != GitFileState::Unmodified {
        let worktree = read_worktree_text(&repository.root.join(&relative_path));
        changes.push(current_file_change(
            CatalogFileChangeId::CurrentWorktreePath(relative_path.clone()),
            relative_path,
            CurrentFileStatus {
                index: GitFileState::Unmodified,
                worktree: status.worktree,
                untracked: false,
            },
            index,
            worktree,
        ));
    }
    changes
}

fn current_file_change(
    id: CatalogFileChangeId,
    relative_path: PathBuf,
    status: CurrentFileStatus,
    baseline: TextSnapshot,
    current: TextSnapshot,
) -> CatalogFileChange {
    let action = if !baseline.exists {
        FileChangeAction::Create
    } else if !current.exists {
        FileChangeAction::Delete
    } else {
        FileChangeAction::Overwrite
    };
    let unavailable_detail = baseline
        .error
        .as_ref()
        .or(current.error.as_ref())
        .map(|error| unavailable(error.kind, error.message.clone()));
    if let Some(detail) = unavailable_detail {
        return CatalogFileChange {
            id,
            path: relative_path,
            action,
            current_status: Some(status),
            summary: None,
            detail,
        };
    }
    if !baseline.exists && !current.exists {
        return CatalogFileChange {
            id,
            path: relative_path,
            action,
            current_status: Some(status),
            summary: None,
            detail: unavailable(
                CatalogUnavailableKind::ReadError,
                "neither side of the change contains text content",
            ),
        };
    }
    let review = build_file_change_review(
        action,
        relative_path.clone(),
        &baseline.content,
        baseline.exists,
        None,
        &current.content,
        current.exists,
    );
    CatalogFileChange {
        id,
        path: relative_path,
        action,
        current_status: Some(status),
        summary: Some(review.summary()),
        detail: CatalogChangeDetail::Available(Box::new(review)),
    }
}

fn is_repository_relative_path(path: &Path) -> bool {
    !path.as_os_str().is_empty()
        && path
            .components()
            .all(|component| matches!(component, Component::Normal(_)))
}

#[derive(Clone)]
struct TextSnapshot {
    exists: bool,
    content: String,
    error: Option<TextSnapshotError>,
}

#[derive(Clone)]
struct TextSnapshotError {
    kind: CatalogUnavailableKind,
    message: String,
}

impl TextSnapshot {
    fn missing() -> Self {
        Self {
            exists: false,
            content: String::new(),
            error: None,
        }
    }

    fn error(exists: bool, kind: CatalogUnavailableKind, message: impl Into<String>) -> Self {
        Self {
            exists,
            content: String::new(),
            error: Some(TextSnapshotError {
                kind,
                message: message.into(),
            }),
        }
    }
}

fn read_head_text(
    repository: &Repository,
    relative_path: &Path,
    status: CurrentFileStatus,
) -> TextSnapshot {
    if !repository.has_head {
        return TextSnapshot::missing();
    }
    read_git_text(
        repository,
        head_object(relative_path),
        "HEAD content",
        status.untracked || status.index == GitFileState::Added,
    )
}

fn read_index_text(
    repository: &Repository,
    relative_path: &Path,
    status: CurrentFileStatus,
) -> TextSnapshot {
    if status.untracked || status.index == GitFileState::Deleted {
        return TextSnapshot::missing();
    }
    read_git_text(
        repository,
        index_object(relative_path),
        "index content",
        false,
    )
}

fn read_git_text(
    repository: &Repository,
    object: OsString,
    source: &str,
    missing_allowed: bool,
) -> TextSnapshot {
    let size = match run_git(
        &repository.root,
        [OsStr::new("cat-file"), OsStr::new("-s"), object.as_os_str()],
    ) {
        Ok(output) if output.status.success() => output,
        Ok(_) if missing_allowed => {
            return TextSnapshot::missing();
        }
        Ok(output) => {
            return TextSnapshot::error(true, CatalogUnavailableKind::GitError, git_error(&output));
        }
        Err(message) => {
            return TextSnapshot::error(true, CatalogUnavailableKind::GitError, message);
        }
    };
    let size = match String::from_utf8_lossy(&size.stdout).trim().parse::<u64>() {
        Ok(size) => size,
        Err(_) => {
            return TextSnapshot::error(
                true,
                CatalogUnavailableKind::GitError,
                format!("git returned an invalid {source} blob size"),
            );
        }
    };
    if size > MAX_CURRENT_TEXT_BYTES {
        return TextSnapshot::error(
            true,
            CatalogUnavailableKind::UnsupportedText,
            format!("{source} exceeds the {MAX_CURRENT_TEXT_BYTES}-byte review limit"),
        );
    }
    let output = match run_git(
        &repository.root,
        [
            OsStr::new("cat-file"),
            OsStr::new("blob"),
            object.as_os_str(),
        ],
    ) {
        Ok(output) => output,
        Err(message) => {
            return TextSnapshot::error(true, CatalogUnavailableKind::GitError, message);
        }
    };
    if !output.status.success() {
        return TextSnapshot::error(true, CatalogUnavailableKind::GitError, git_error(&output));
    }
    if output.stdout.len() as u64 != size {
        return TextSnapshot::error(
            true,
            CatalogUnavailableKind::GitError,
            format!(
                "git returned {} bytes for a {size}-byte {source} blob",
                output.stdout.len()
            ),
        );
    }
    decode_text(output.stdout, source)
}

fn head_object(relative_path: &Path) -> OsString {
    let mut object = b"HEAD:".to_vec();
    object.extend_from_slice(relative_path.as_os_str().as_bytes());
    OsString::from_vec(object)
}

fn index_object(relative_path: &Path) -> OsString {
    let mut object = b":".to_vec();
    object.extend_from_slice(relative_path.as_os_str().as_bytes());
    OsString::from_vec(object)
}

fn read_worktree_text(path: &Path) -> TextSnapshot {
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return TextSnapshot::missing();
        }
        Err(error) => {
            return TextSnapshot::error(
                true,
                CatalogUnavailableKind::ReadError,
                format!("cannot inspect working-tree file: {error}"),
            );
        }
    };
    if !metadata.is_file() {
        return TextSnapshot::error(
            true,
            CatalogUnavailableKind::UnsupportedText,
            "working-tree path is not a regular file",
        );
    }
    if metadata.len() > MAX_CURRENT_TEXT_BYTES {
        return TextSnapshot::error(
            true,
            CatalogUnavailableKind::UnsupportedText,
            format!("working-tree content exceeds the {MAX_CURRENT_TEXT_BYTES}-byte review limit"),
        );
    }
    let mut file = match OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
    {
        Ok(file) => file,
        Err(error) => {
            return TextSnapshot::error(
                true,
                CatalogUnavailableKind::ReadError,
                format!("cannot open working-tree file: {error}"),
            );
        }
    };
    let opened_metadata = match file.metadata() {
        Ok(metadata) if metadata.is_file() => metadata,
        Ok(_) => {
            return TextSnapshot::error(
                true,
                CatalogUnavailableKind::UnsupportedText,
                "working-tree path changed to a non-file",
            );
        }
        Err(error) => {
            return TextSnapshot::error(
                true,
                CatalogUnavailableKind::ReadError,
                format!("cannot inspect opened working-tree file: {error}"),
            );
        }
    };
    if FileIdentity::from_metadata(&metadata) != FileIdentity::from_metadata(&opened_metadata) {
        return TextSnapshot::error(
            true,
            CatalogUnavailableKind::ReadError,
            "working-tree file changed while the catalog was loading",
        );
    }
    let mut bytes = Vec::with_capacity(opened_metadata.len() as usize);
    if let Err(error) = Read::by_ref(&mut file)
        .take(MAX_CURRENT_TEXT_BYTES + 1)
        .read_to_end(&mut bytes)
    {
        return TextSnapshot::error(
            true,
            CatalogUnavailableKind::ReadError,
            format!("cannot read working-tree file: {error}"),
        );
    }
    let final_metadata = match file.metadata() {
        Ok(metadata) => metadata,
        Err(error) => {
            return TextSnapshot::error(
                true,
                CatalogUnavailableKind::ReadError,
                format!("cannot recheck working-tree file: {error}"),
            );
        }
    };
    if FileIdentity::from_metadata(&opened_metadata) != FileIdentity::from_metadata(&final_metadata)
        || bytes.len() as u64 != opened_metadata.len()
    {
        return TextSnapshot::error(
            true,
            CatalogUnavailableKind::ReadError,
            "working-tree file changed while the catalog was loading",
        );
    }
    decode_text(bytes, "working-tree content")
}

fn decode_text(bytes: Vec<u8>, source: &str) -> TextSnapshot {
    if bytes.contains(&0) {
        return TextSnapshot::error(
            true,
            CatalogUnavailableKind::UnsupportedText,
            format!("{source} is binary"),
        );
    }
    match String::from_utf8(bytes) {
        Ok(content) => TextSnapshot {
            exists: true,
            content,
            error: None,
        },
        Err(_) => TextSnapshot::error(
            true,
            CatalogUnavailableKind::UnsupportedText,
            format!("{source} is not UTF-8"),
        ),
    }
}

fn run_git<I, S>(directory: &Path, arguments: I) -> Result<Output, String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    Command::new("git")
        .arg("--no-optional-locks")
        .arg("-c")
        .arg("core.fsmonitor=false")
        .arg("-C")
        .arg(directory)
        .args(arguments)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("GIT_PAGER", "cat")
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .env_remove("GIT_COMMON_DIR")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .map_err(|error| format!("cannot run read-only git query: {error}"))
}

fn git_error(output: &Output) -> String {
    let message = String::from_utf8_lossy(&output.stderr).trim().to_owned();
    if message.is_empty() {
        format!("read-only git query failed with {}", output.status)
    } else {
        message
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use async_trait::async_trait;
    use serde_json::json;
    use tokio::sync::mpsc::UnboundedSender;
    use tokio_util::sync::CancellationToken;

    use super::*;
    use tickloop_agent_model::{ModelClient, ModelDelta, ModelError, ModelOutput, ModelRequest};
    use tickloop_agent_protocol::OperationRecord;
    use tickloop_agent_protocol::PreparedFileChange;
    use tickloop_agent_protocol::{InputModality, Message, ModelConfig, Protocol};
    use tickloop_agent_runtime::Agent;
    use tickloop_agent_tools::{Tools, Workspace};

    struct UnusedModel;

    #[async_trait]
    impl ModelClient for UnusedModel {
        async fn generate_title(
            &self,
            request: tickloop_agent_model::ModelRequest,
            _: tokio::sync::mpsc::UnboundedSender<tickloop_agent_model::ModelDelta>,
            _: tokio_util::sync::CancellationToken,
        ) -> Result<tickloop_agent_model::ModelOutput, tickloop_agent_model::ModelError> {
            Ok(tickloop_agent_model::fixture_title(request))
        }

        fn identity(&self) -> tickloop_agent_protocol::ModelIdentity {
            tickloop_agent_protocol::ModelIdentity {
                provider: tickloop_agent_protocol::ModelProvider::Api,
                profile: "fixture".into(),
                protocol: tickloop_agent_protocol::Protocol::Messages,
                requested_model: "fixture".into(),
            }
        }

        async fn complete(
            &self,
            _: ModelRequest,
            _: UnboundedSender<ModelDelta>,
            _: CancellationToken,
        ) -> Result<ModelOutput, ModelError> {
            panic!("catalog loading must not call the model")
        }
    }

    fn model_config() -> ModelConfig {
        ModelConfig {
            provider: tickloop_agent_protocol::ModelProvider::Api,
            auth_source: tickloop_agent_protocol::ModelAuthSource::ApiKeyEnv,
            name: "catalog-test".into(),
            protocol: Protocol::Messages,
            base_url: "http://127.0.0.1".into(),
            model: "unused".into(),
            api_key_env: String::new(),
            max_output_tokens: 64,
            provider_id: String::new(),
            provider_name: String::new(),
            model_id: String::new(),
            connection: None,
            card: None,
            input_modalities: vec![InputModality::Text],
        }
    }

    fn fixture_git(directory: &Path, arguments: &[&str]) {
        let output = Command::new("git")
            .arg("-C")
            .arg(directory)
            .args(arguments)
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {:?} failed: {}",
            arguments,
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn initialize_repository(directory: &Path) {
        fixture_git(directory, &["init", "-q"]);
        std::fs::write(directory.join("tracked.txt"), "base\n").unwrap();
        std::fs::write(directory.join("deleted.txt"), "remove me\n").unwrap();
        fixture_git(directory, &["add", "tracked.txt", "deleted.txt"]);
        fixture_git(
            directory,
            &[
                "-c",
                "user.name=Tickloop",
                "-c",
                "user.email=tickloop@example.invalid",
                "-c",
                "commit.gpgsign=false",
                "commit",
                "-qm",
                "baseline",
            ],
        );
    }

    #[derive(Clone, Copy)]
    enum CurrentLayer {
        Index,
        Worktree,
    }

    fn current_file<'a>(
        source: &'a ChangeSource,
        path: &str,
        layer: CurrentLayer,
    ) -> &'a CatalogFileChange {
        let expected_id = match layer {
            CurrentLayer::Index => CatalogFileChangeId::CurrentIndexPath(PathBuf::from(path)),
            CurrentLayer::Worktree => CatalogFileChangeId::CurrentWorktreePath(PathBuf::from(path)),
        };
        source
            .files
            .iter()
            .find(|file| file.id == expected_id)
            .unwrap_or_else(|| panic!("missing catalog entry {path}"))
    }

    fn metadata_fingerprint(path: &Path) -> (u64, u64, i64, i64) {
        use std::os::unix::fs::MetadataExt;

        let metadata = std::fs::metadata(path).unwrap();
        (
            metadata.len(),
            metadata.ino(),
            metadata.mtime(),
            metadata.mtime_nsec(),
        )
    }

    async fn catalog_for(directory: &Path) -> ChangeCatalog {
        let agent = Agent::new_embedded(model_config(), Arc::new(UnusedModel));
        let chat = agent.create_chat(directory.to_path_buf()).await.unwrap();
        let snapshot = agent.snapshot(chat).await.unwrap();
        let catalog = ChangeCatalog::load(&snapshot).await;
        agent.shutdown().await;
        catalog
    }

    #[tokio::test]
    async fn current_source_distinguishes_non_git_clean_and_git_error() {
        let non_git = tempfile::tempdir().unwrap();
        assert_eq!(
            catalog_for(non_git.path()).await.sources[0].state,
            ChangeSourceState::NonGit
        );

        let clean = tempfile::tempdir().unwrap();
        initialize_repository(clean.path());
        let catalog = catalog_for(clean.path()).await;
        let source = &catalog.sources[0];
        assert_eq!(source.state, ChangeSourceState::Clean);
        assert!(source.files.is_empty());

        let broken = tempfile::tempdir().unwrap();
        std::fs::write(broken.path().join(".git"), "gitdir: missing\n").unwrap();
        assert!(matches!(
            catalog_for(broken.path()).await.sources[0].state,
            ChangeSourceState::GitError { .. }
        ));
    }

    #[tokio::test]
    async fn current_source_combines_staged_unstaged_deleted_and_untracked_text() {
        let directory = tempfile::tempdir().unwrap();
        initialize_repository(directory.path());
        std::fs::write(directory.path().join("tracked.txt"), "staged\n").unwrap();
        fixture_git(directory.path(), &["add", "tracked.txt"]);
        std::fs::write(directory.path().join("tracked.txt"), "working\n").unwrap();
        std::fs::write(directory.path().join("staged-new.txt"), "staged new\n").unwrap();
        fixture_git(directory.path(), &["add", "staged-new.txt"]);
        std::fs::write(directory.path().join("untracked.txt"), "untracked\n").unwrap();
        std::fs::write(directory.path().join("line\nbreak.txt"), "newline path\n").unwrap();
        std::fs::write(directory.path().join("binary.bin"), [0, 1, 2]).unwrap();
        std::fs::remove_file(directory.path().join("deleted.txt")).unwrap();
        let index_before = std::fs::read(directory.path().join(".git/index")).unwrap();

        let catalog = catalog_for(directory.path()).await;
        let source = &catalog.sources[0];

        assert_eq!(source.state, ChangeSourceState::Ready);
        assert_eq!(source.files.len(), 7);
        assert_eq!(
            std::fs::read(directory.path().join(".git/index")).unwrap(),
            index_before
        );
        assert!(!directory.path().join(".git/index.lock").exists());

        let tracked_index = current_file(source, "tracked.txt", CurrentLayer::Index);
        assert_eq!(tracked_index.action, FileChangeAction::Overwrite);
        assert_eq!(
            tracked_index.current_status,
            Some(CurrentFileStatus {
                index: GitFileState::Modified,
                worktree: GitFileState::Unmodified,
                untracked: false,
            })
        );
        let CatalogChangeDetail::Available(review) = &tracked_index.detail else {
            panic!("tracked index diff should be available")
        };
        assert_eq!(
            review.before.sha256,
            tracked_index.summary.as_ref().unwrap().before.sha256
        );
        assert!(
            review
                .hunks
                .iter()
                .flat_map(|hunk| &hunk.lines)
                .any(|line| line.kind
                    == tickloop_agent_tools::file_change::FileChangeLineKind::Added
                    && line.text == "staged")
        );

        let tracked_worktree = current_file(source, "tracked.txt", CurrentLayer::Worktree);
        assert_eq!(
            tracked_worktree.current_status,
            Some(CurrentFileStatus {
                index: GitFileState::Unmodified,
                worktree: GitFileState::Modified,
                untracked: false,
            })
        );
        let CatalogChangeDetail::Available(review) = &tracked_worktree.detail else {
            panic!("tracked worktree diff should be available")
        };
        assert!(
            review
                .hunks
                .iter()
                .flat_map(|hunk| &hunk.lines)
                .any(|line| {
                    line.kind == tickloop_agent_tools::file_change::FileChangeLineKind::Removed
                        && line.text == "staged"
                })
        );
        assert!(
            review
                .hunks
                .iter()
                .flat_map(|hunk| &hunk.lines)
                .any(|line| {
                    line.kind == tickloop_agent_tools::file_change::FileChangeLineKind::Added
                        && line.text == "working"
                })
        );

        let staged = current_file(source, "staged-new.txt", CurrentLayer::Index);
        assert_eq!(staged.action, FileChangeAction::Create);
        assert_eq!(staged.current_status.unwrap().index, GitFileState::Added);
        assert!(!staged.current_status.unwrap().untracked);

        let untracked = current_file(source, "untracked.txt", CurrentLayer::Worktree);
        assert_eq!(untracked.action, FileChangeAction::Create);
        assert!(untracked.current_status.unwrap().untracked);
        assert!(matches!(
            untracked.detail,
            CatalogChangeDetail::Available(_)
        ));
        assert!(
            current_file(source, "line\nbreak.txt", CurrentLayer::Worktree)
                .current_status
                .unwrap()
                .untracked
        );

        let deleted = current_file(source, "deleted.txt", CurrentLayer::Worktree);
        assert_eq!(deleted.action, FileChangeAction::Delete);
        assert_eq!(
            deleted.current_status.unwrap().worktree,
            GitFileState::Deleted
        );
        let CatalogChangeDetail::Available(review) = &deleted.detail else {
            panic!("deleted text diff should be available")
        };
        assert!(review.before.exists);
        assert!(!review.after.exists);

        let binary = current_file(source, "binary.bin", CurrentLayer::Worktree);
        assert!(binary.current_status.unwrap().untracked);
        assert!(matches!(
            binary.detail,
            CatalogChangeDetail::Unavailable {
                kind: CatalogUnavailableKind::UnsupportedText,
                ..
            }
        ));
    }

    #[tokio::test]
    async fn current_source_keeps_index_and_worktree_when_they_cancel_against_head() {
        let directory = tempfile::tempdir().unwrap();
        initialize_repository(directory.path());
        std::fs::write(directory.path().join("tracked.txt"), "staged\n").unwrap();
        fixture_git(directory.path(), &["add", "tracked.txt"]);
        std::fs::write(directory.path().join("tracked.txt"), "base\n").unwrap();
        let index_path = directory.path().join(".git/index");
        let index_bytes_before = std::fs::read(&index_path).unwrap();
        let index_metadata_before = metadata_fingerprint(&index_path);

        let catalog = catalog_for(directory.path()).await;
        let source = &catalog.sources[0];

        assert_eq!(source.files.len(), 2);
        assert_eq!(std::fs::read(&index_path).unwrap(), index_bytes_before);
        assert_eq!(metadata_fingerprint(&index_path), index_metadata_before);
        assert!(!directory.path().join(".git/index.lock").exists());

        let staged = current_file(source, "tracked.txt", CurrentLayer::Index);
        let working = current_file(source, "tracked.txt", CurrentLayer::Worktree);
        assert_eq!(staged.summary.as_ref().unwrap().added_lines, 1);
        assert_eq!(staged.summary.as_ref().unwrap().removed_lines, 1);
        assert_eq!(working.summary.as_ref().unwrap().added_lines, 1);
        assert_eq!(working.summary.as_ref().unwrap().removed_lines, 1);

        let CatalogChangeDetail::Available(staged_review) = &staged.detail else {
            panic!("staged diff should be available")
        };
        let CatalogChangeDetail::Available(working_review) = &working.detail else {
            panic!("worktree diff should be available")
        };
        assert!(
            staged_review
                .hunks
                .iter()
                .flat_map(|hunk| &hunk.lines)
                .any(|line| {
                    line.kind == tickloop_agent_tools::file_change::FileChangeLineKind::Added
                        && line.text == "staged"
                })
        );
        assert!(
            working_review
                .hunks
                .iter()
                .flat_map(|hunk| &hunk.lines)
                .any(|line| {
                    line.kind == tickloop_agent_tools::file_change::FileChangeLineKind::Removed
                        && line.text == "staged"
                })
        );
    }

    fn operation(
        run_id: RunId,
        state: OperationState,
        file_change: PreparedFileChange,
    ) -> OperationRecord {
        OperationRecord {
            task_binding: None,
            id: uuid::Uuid::new_v4(),
            loop_id: uuid::Uuid::new_v4(),
            run_id,
            call_id: uuid::Uuid::new_v4().to_string(),
            name: "Write".into(),
            input: json!({"file_path":file_change.summary.path}),
            file_change: Some(file_change),
            state,
            result: None,
        }
    }

    async fn prepared_change(
        tools: &Tools,
        workspace: &Workspace,
        path: &str,
    ) -> PreparedFileChange {
        tools
            .prepare(
                workspace,
                "Write",
                &json!({"file_path":path,"content":format!("content for {path}\n")}),
            )
            .await
            .unwrap()
            .file_change
            .unwrap()
    }

    #[tokio::test]
    async fn agent_sources_group_completed_operations_and_localize_bad_artifacts() {
        let directory = tempfile::tempdir().unwrap();
        let output = tempfile::tempdir().unwrap();
        let agent = Agent::new_embedded(model_config(), Arc::new(UnusedModel));
        let chat = agent.create_chat(directory.path().into()).await.unwrap();
        let mut snapshot = agent.snapshot(chat).await.unwrap();
        let tools = Tools::new(output.path().join("catalog-output"));
        let workspace = Workspace {
            workdir: directory.path().to_path_buf(),
            additional_directories: Vec::new(),
        };
        let available = prepared_change(&tools, &workspace, "available.txt").await;
        let missing = prepared_change(&tools, &workspace, "missing.txt").await;
        let corrupt = prepared_change(&tools, &workspace, "corrupt.txt").await;
        let failed = prepared_change(&tools, &workspace, "failed.txt").await;
        std::fs::remove_file(&missing.artifact.path).unwrap();
        let mut corrupt_bytes = std::fs::read(&corrupt.artifact.path).unwrap();
        corrupt_bytes[0] ^= 1;
        std::fs::write(&corrupt.artifact.path, corrupt_bytes).unwrap();

        let first_run = uuid::Uuid::new_v4();
        let failed_run = uuid::Uuid::new_v4();
        let second_run = uuid::Uuid::new_v4();
        snapshot.messages = vec![
            Message {
                role: Role::User,
                content: vec![Content::Text {
                    text: "first request".into(),
                }],
            },
            Message {
                role: Role::User,
                content: vec![Content::Text {
                    text: "second request".into(),
                }],
            },
        ];
        snapshot.message_runs.insert(0, first_run);
        snapshot.message_runs.insert(1, second_run);
        snapshot.operations = vec![
            operation(first_run, OperationState::Completed, available),
            operation(failed_run, OperationState::Failed, failed),
            operation(first_run, OperationState::Completed, missing),
            operation(second_run, OperationState::Completed, corrupt),
        ];

        let catalog = ChangeCatalog::load(&snapshot).await;

        assert_eq!(catalog.sources[0].id, ChangeSourceId::Current);
        assert_eq!(catalog.sources[0].state, ChangeSourceState::NonGit);
        assert_eq!(catalog.sources.len(), 3);
        let first = &catalog.sources[1];
        assert_eq!(first.id, ChangeSourceId::AgentRun(first_run));
        assert_eq!(first.user_prompt.as_deref(), Some("first request"));
        assert_eq!(first.files.len(), 2);
        assert!(matches!(
            first.files[0].detail,
            CatalogChangeDetail::Available(_)
        ));
        assert!(matches!(
            first.files[1].detail,
            CatalogChangeDetail::Unavailable {
                kind: CatalogUnavailableKind::MissingArtifact,
                ..
            }
        ));
        let second = &catalog.sources[2];
        assert_eq!(second.id, ChangeSourceId::AgentRun(second_run));
        assert_eq!(second.user_prompt.as_deref(), Some("second request"));
        assert_eq!(second.files.len(), 1);
        assert!(matches!(
            second.files[0].detail,
            CatalogChangeDetail::Unavailable {
                kind: CatalogUnavailableKind::CorruptArtifact,
                ..
            }
        ));
        assert!(
            catalog
                .sources
                .iter()
                .all(|source| source.id != ChangeSourceId::AgentRun(failed_run))
        );
        agent.shutdown().await;
    }
}
