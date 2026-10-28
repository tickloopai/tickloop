use std::{
    fs::File,
    io::Read,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use crate::file_change::{ARTIFACT_DIRECTORY, FileChangeArtifactError, persist_file_change_review};
use tickloop_harness_base::git_worktree::sibling_linked_worktrees;
use tickloop_harness_base::image::ImageError;
use tickloop_harness_base::network::Reach;
use tickloop_harness_base::private_fs::{
    OpenTarget, PrivateDirectory, open_without_following_links,
};
use tickloop_harness_protocol::{
    CommandSnapshot, CommandState, FileReadRecord, Image, PreparedFileChange, SearchConfig,
    ToolDefinition, ToolResult,
};

pub mod file_change;
pub mod permissions;
mod persisted_output;
pub use persisted_output::{
    PERSISTED_OUTPUT_TAG, PREVIEW_LABEL, SAVED_TO_LABEL, generate_preview, large_output_message,
    utf16_length,
};
mod read;
pub use read::{FileReadRecordExt, FileReadState, READ_TOOL_NAME};
mod search;
pub use search::{GLOB_TOOL_NAME, GREP_TOOL_NAME};
mod shell;
pub use shell::CANCELLED_COMMAND_REASON;
pub use shell::{
    BASH_TOOL_NAME, PreparedShellStart, PreparedShellStop, ShellExit, ShellHost, ShellLaunch,
    ShellPreparation, ShellPreparationRequest, ShellProcess, ShellRetention, ShellRuntime,
    ShellStartError, ShellStopAdmission, ShellStopError, ShellStopPreparation, TASK_STOP_TOOL_NAME,
    UserCommandSnapshot,
};
mod web;
pub mod web_provenance;
pub mod web_search;
pub use web::SecondaryModel;
pub use web::{WEB_FETCH_TOOL_NAME, WEB_SEARCH_TOOL_NAME};
mod write;
pub use write::{EDIT_TOOL_NAME, WRITE_TOOL_NAME};

#[cfg(test)]
mod file_change_review_tests;
#[cfg(test)]
mod file_write_publication_tests;
#[cfg(test)]
mod read_tests;
#[cfg(test)]
mod search_tests;
#[cfg(test)]
mod write_tests;

const DEFAULT_OUTPUT_BYTES: usize = 16 * 1024;
const MAX_OUTPUT_BYTES: usize = 64 * 1024;
pub const TASK_OUTPUT_SUBDIRECTORY: &str = "tasks";
pub const SCRATCHPAD_SUBDIRECTORY: &str = "scratchpad";
/// Why a tool that works on local files or commands refuses a Chat without a Folder.
const FOLDERLESS_REJECTION: &str =
    "This Chat has no Folder, so it cannot read or change files or run commands.";

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Workspace {
    /// The Chat's Folder; `None` for a Chat without one, which prepares only the network tools.
    pub folder: Option<PathBuf>,
    pub additional_directories: Vec<PathBuf>,
}

impl Workspace {
    /// Whether the canonical `path` lies in the canonical form of a workspace directory.
    fn contains(&self, path: &Path) -> bool {
        self.folder
            .iter()
            .chain(&self.additional_directories)
            .filter_map(|directory| directory.canonicalize().ok())
            .any(|directory| path.starts_with(directory))
    }

    /// This workspace plus the other linked worktrees of the Folder's repository, which Auto
    /// counts as the same project; `None` when the repository has none.
    fn with_sibling_worktrees(&self) -> Option<Workspace> {
        let siblings = sibling_linked_worktrees(self.folder.as_deref()?);
        (!siblings.is_empty()).then(|| Workspace {
            folder: self.folder.clone(),
            additional_directories: self
                .additional_directories
                .iter()
                .cloned()
                .chain(siblings)
                .collect(),
        })
    }

    /// `path` below the workspace directory that holds it, as written or in canonical form, or
    /// the whole path when no directory does.
    fn below_directory<'a>(&self, path: &'a Path) -> &'a Path {
        self.folder
            .iter()
            .chain(&self.additional_directories)
            .flat_map(|directory| [Some(directory.clone()), directory.canonicalize().ok()])
            .flatten()
            .find_map(|directory| path.strip_prefix(directory).ok())
            .unwrap_or(path)
    }
}

/// A local file a host tool sends elsewhere, read whole by the `Read` path rules: it needs no
/// approval in the workspace or this Chat's own directories and approval anywhere else. `read`
/// is that `Read`, which the Chat admitted before the file was opened.
#[derive(Clone, Debug)]
pub struct LocalFile {
    pub path: PathBuf,
    pub read: PreparedTool,
    /// An explicit Ask rule covers the `Read`, so the call's one approval must come from a person.
    pub read_asks: bool,
    pub bytes: Vec<u8>,
}

/// The Chat's decision on a `Read` of a local file a host tool would send elsewhere, made after the
/// path is resolved and before the file is opened: `Ok(true)` admits it under an explicit Ask, and
/// `Err` says why it is refused.
#[async_trait::async_trait]
pub trait LocalReadAdmission: Send + Sync {
    async fn admit(&self, read: &PreparedTool) -> Result<bool, String>;
}

/// Read-only access to one Chat's local files, lent to a host tool that sends a file elsewhere: it
/// reads whole files by that Chat's `Read` path rules in its workspace, only once the Chat admits
/// each `Read`, and can neither write files nor run anything.
#[derive(Clone)]
pub struct LocalFileReader {
    workspace: Workspace,
    readable_directories: Vec<PathBuf>,
    family_temp: PathBuf,
    admission: std::sync::Arc<dyn LocalReadAdmission>,
}

impl LocalFileReader {
    /// Resolves `file_path`, has the Chat admit its `Read`, and only then opens the file and reads
    /// at most `max_bytes` of it. A path that looks like it holds credentials or secrets is
    /// refused, whatever the approval.
    pub async fn read(&self, file_path: &str, max_bytes: usize) -> Result<LocalFile, ToolError> {
        let reader = self.clone();
        let requested = file_path.to_owned();
        let read = run_blocking(file_path, move || reader.resolve(&requested)).await?;
        let read_asks = self
            .admission
            .admit(&read)
            .await
            .map_err(ToolError::Rejected)?;
        let workspace = self.workspace.clone();
        let (read, bytes) = run_blocking(file_path, move || {
            if [&read.original_path, &read.resolved_path]
                .into_iter()
                .any(|path| is_sensitive_path(workspace.below_directory(path)))
            {
                return Err(ToolError::Rejected(format!(
                    "{} looks like it holds credentials or secrets, so its contents are never sent",
                    read.original_path.display()
                )));
            }
            let bytes = read_whole_file(&read.resolved_path, max_bytes)?;
            Ok((read, bytes))
        })
        .await?;
        Ok(LocalFile {
            path: read.resolved_path.clone(),
            read,
            read_asks,
            bytes,
        })
    }

    fn resolve(&self, file_path: &str) -> Result<PreparedTool, ToolError> {
        prepare_with_readable_directories(
            &self.workspace,
            READ_TOOL_NAME,
            &json!({"file_path": file_path}),
            &self.readable_directories,
            Some(&self.family_temp),
            &FileReadState::default(),
        )
    }
}

async fn run_blocking<T: Send + 'static>(
    file_path: &str,
    work: impl FnOnce() -> Result<T, ToolError> + Send + 'static,
) -> Result<T, ToolError> {
    tokio::task::spawn_blocking(work)
        .await
        .map_err(|error| ToolError::Io {
            path: file_path.into(),
            reason: format!("file read did not finish: {error}"),
        })?
}

/// A write Claude Code never admits through a fast path, following its
/// `safetyCheck` permission result.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SafetyCheck {
    pub reason: String,
    /// Whether Auto may still hand the call to its classifier; otherwise only
    /// the user can approve it.
    pub classifier_approvable: bool,
}

// The runtime implements these tools itself; their names live here so that Auto's allowlist
// names them without depending on the runtime.
pub const ENTER_PLAN_MODE_TOOL_NAME: &str = "EnterPlanMode";
pub const EXIT_PLAN_MODE_TOOL_NAME: &str = "ExitPlanMode";
/// The input of the `ExitPlanMode` permission request that asks the user to leave Plan mode
/// without a plan, Claude Code's "Exit plan mode?" dialog; a plan approval names its
/// `plan_version` instead.
pub const EXIT_PLAN_MODE_WITHOUT_PLAN: &str = "exit_without_plan";

/// Whether a permission request asks to leave Plan mode without a plan.
pub fn exits_plan_mode_without_plan(name: &str, input: &serde_json::Value) -> bool {
    name == EXIT_PLAN_MODE_TOOL_NAME && input[EXIT_PLAN_MODE_WITHOUT_PLAN] == true
}
pub const TODO_WRITE_TOOL_NAME: &str = "TodoWrite";
pub const ASK_USER_QUESTION_TOOL_NAME: &str = "AskUserQuestion";
/// The result of an `AskUserQuestion` call the user declined: Claude Code's `REJECT_MESSAGE`,
/// which its question dialog sends on Esc. The runtime writes it and the Console recognizes it.
pub const DECLINED_QUESTIONS_RESULT: &str = "The user doesn't want to proceed with this tool use. The tool use was rejected (eg. if it was a file edit, the new_string was NOT written to the file). STOP what you are doing and wait for the user to tell you how to proceed.";

/// Tools Auto admits without the classifier, mapped from Claude Code's
/// `SAFE_YOLO_ALLOWLISTED_TOOLS`.
pub const AUTO_ALLOWLISTED_TOOLS: [&str; 7] = [
    READ_TOOL_NAME,
    GLOB_TOOL_NAME,
    GREP_TOOL_NAME,
    ENTER_PLAN_MODE_TOOL_NAME,
    EXIT_PLAN_MODE_TOOL_NAME,
    TASK_STOP_TOOL_NAME,
    ASK_USER_QUESTION_TOOL_NAME,
];

#[derive(Clone, Debug)]
pub struct PreparedTool {
    pub name: String,
    pub input: Value,
    pub requires_permission: bool,
    pub is_read_only: bool,
    pub in_workspace: bool,
    pub file_change: Option<PreparedFileChange>,
    /// Outside the workspace but inside another linked worktree of the Folder's repository.
    in_sibling_worktree: bool,
    in_family_temp: bool,
    automatic_shell_workspace: Option<Workspace>,
    original_path: PathBuf,
    resolved_path: PathBuf,
    operation: Operation,
}

#[derive(Clone, Debug)]
enum Operation {
    Write(Box<write::PreparedWrite>),
    Shell(shell::ShellInput),
    TaskStop(shell::TaskStopInput),
    WebFetch(web::FetchInput),
    WebSearch(web::SearchInput),
    Read(read::ReadRequest),
    Search(search::SearchRequest),
}

impl Operation {
    /// The input field that names the operation's path, or `None` for operations without one.
    fn path_field(&self) -> Option<&'static str> {
        match self {
            Operation::WebFetch(_) | Operation::WebSearch(_) | Operation::TaskStop(_) => None,
            Operation::Read(_) | Operation::Write(_) => Some("file_path"),
            Operation::Shell(_) => Some("cwd"),
            Operation::Search(_) => Some("path"),
        }
    }

    /// Whether the tool's own permission check can ask for approval. Claude Code's `TaskStop` keeps
    /// the default `checkPermissions`, which allows every call and leaves only explicit rules.
    fn has_permission_check(&self) -> bool {
        !matches!(self, Operation::TaskStop(_))
    }

    /// Resolves the operation's path to the canonical target it acts on. A write may name a file
    /// that does not exist yet, and a new file may also be missing parent directories.
    fn resolve_target(&self, path: &Path, is_read_only: bool) -> Result<PathBuf, ToolError> {
        match self {
            Operation::Write(_) => resolve_write_target(path),
            _ => resolve_path(path, !is_read_only),
        }
    }
}

#[derive(Debug)]
pub struct ToolExecution {
    pub result: Result<ToolResult, ToolError>,
    /// Images for the model only; `result` carries the text shown in the interface.
    pub images: Vec<Image>,
    /// Structured web evidence before the runtime assigns family-wide citation numbers.
    pub web_sources: Vec<WebSource>,
}

impl From<Result<ToolResult, ToolError>> for ToolExecution {
    fn from(result: Result<ToolResult, ToolError>) -> Self {
        Self {
            result,
            images: Vec::new(),
            web_sources: Vec::new(),
        }
    }
}

#[derive(Clone, Debug)]
pub struct WebSource {
    pub url: String,
    pub title: String,
    pub snippet: String,
}

/// What a tool execution knows about the run it serves.
#[derive(Clone, Copy, Debug)]
pub struct ExecutionContext {
    /// How the Loop writes ids into the result and reads the ids the model passed.
    pub ids: tickloop_ids::IdContext,
    pub operation_id: uuid::Uuid,
    pub loop_id: uuid::Uuid,
    /// Whether the model reading the result accepts images; `Read` refuses images otherwise.
    pub accepts_images: bool,
}

impl PreparedTool {
    pub fn requires_environment_approval(&self) -> bool {
        matches!(&self.operation, Operation::Shell(input)
            if input.preparation.as_ref().is_some_and(ShellPreparation::requires_approval))
    }

    pub fn skips_approval_by_isolation(&self) -> bool {
        matches!(&self.operation, Operation::Shell(input)
            if input.preparation.as_ref().is_some_and(ShellPreparation::skips_approval_by_isolation))
    }

    pub fn approval_facts(&self) -> tickloop_harness_protocol::PreparedApprovalFacts {
        tickloop_harness_protocol::PreparedApprovalFacts {
            requires_approval: self.requires_environment_approval().then_some(true),
            isolation: match &self.operation {
                Operation::Shell(input) => {
                    input.preparation.as_ref().map(ShellPreparation::isolation)
                }
                _ => None,
            },
            normalized_input: self.input.clone(),
            canonical_path: self
                .operation
                .path_field()
                .map(|_| self.resolved_path.clone()),
            in_workspace: Some(self.in_workspace),
            read_only: Some(self.is_read_only),
            ..Default::default()
        }
    }
    /// Repeats the family-temp admission right before the command runs, on a freshly resolved
    /// cwd that must still be the prepared one.
    fn revalidate_automatic_shell_paths(&self) -> Result<(), ToolError> {
        let (Operation::Shell(input), Some(workspace)) =
            (&self.operation, &self.automatic_shell_workspace)
        else {
            return Ok(());
        };
        let cwd = self
            .operation
            .resolve_target(&self.original_path, self.is_read_only)?;
        if cwd != self.resolved_path || !input.is_allowed_with_family_temp(&cwd, workspace) {
            return Err(ToolError::PathChanged(self.original_path.clone()));
        }
        Ok(())
    }

    pub fn is_supported_in_delegated_scope(&self) -> bool {
        match &self.operation {
            Operation::WebFetch(_) | Operation::WebSearch(_) | Operation::TaskStop(_) => true,
            Operation::Shell(_) => true,
            _ => {
                self.in_workspace
                    || self.in_family_temp
                    || self.is_read_only && !self.requires_permission
            }
        }
    }

    /// Returns the resolved file edited by write tools, or `None` for other tools.
    pub fn edited_file_path(&self) -> Option<&Path> {
        matches!(self.operation, Operation::Write(_)).then_some(self.resolved_path.as_path())
    }

    /// Whether this `Read`, `Write` or `Edit` resolves to exactly `path`.
    pub fn accesses_file(&self, path: &Path) -> bool {
        matches!(self.operation, Operation::Read(_) | Operation::Write(_))
            && self.resolved_path == path
    }

    /// Whether Plan mode refuses this call: it changes something and ordinarily needs approval.
    pub fn is_plan_mode_side_effect(&self) -> bool {
        match self.operation {
            Operation::Write(_) | Operation::Shell(_) => !self.is_read_only,
            _ => !self.is_read_only && self.requires_permission,
        }
    }

    pub fn safety_check(&self) -> Option<SafetyCheck> {
        if self.in_family_temp {
            return None;
        }
        match self.operation {
            Operation::Write(_) => shell::auto_edit_safety_check(&self.original_path)
                .or_else(|| shell::auto_edit_safety_check(&self.resolved_path)),
            _ => None,
        }
    }

    pub fn is_allowed_in_accept_edits(&self, workspace: &Workspace) -> bool {
        if !self.in_workspace {
            return false;
        }
        match &self.operation {
            Operation::Write(_) => self.safety_check().is_none(),
            Operation::Shell(input) => {
                input.is_allowed_in_accept_edits(&self.resolved_path, workspace)
            }
            _ => false,
        }
    }

    /// Accept Edits admission with the other linked worktrees of the Folder's repository counted
    /// as workspace directories, which Auto applies before asking its classifier.
    pub fn is_allowed_in_accept_edits_with_sibling_worktrees(&self, workspace: &Workspace) -> bool {
        if !self.in_workspace && !self.in_sibling_worktree {
            return false;
        }
        match &self.operation {
            Operation::Write(_) => self.safety_check().is_none(),
            Operation::Shell(input) => workspace
                .with_sibling_worktrees()
                .is_some_and(|scope| input.is_allowed_in_accept_edits(&self.resolved_path, &scope)),
            _ => false,
        }
    }

    /// Whether Accept Edits admission alone decides this call. Claude Code's Accept Edits shell
    /// checks already include its read-only rules but still ask for read-only `sed` forms, so a
    /// mode-independent read-only admission must not allow the call afterwards.
    pub fn accept_edits_supersedes_read_only(&self) -> bool {
        self.in_workspace && matches!(self.operation, Operation::Shell(_))
    }
}

const SENSITIVE_PATH_NAMES: &[&str] = &[
    ".ssh",
    ".aws",
    ".gnupg",
    ".kube",
    ".docker",
    ".netrc",
    ".npmrc",
    ".pypirc",
    ".git-credentials",
    ".env",
];
const SENSITIVE_PATH_SIGNALS: &[&str] = &[
    "credential",
    "secret",
    "token",
    "keychain",
    "kubeconfig",
    "sudoers",
    "sshd_config",
    "authorized_keys",
];
const SENSITIVE_PATH_EXTENSIONS: &[&str] = &["pem", "key", "p12", "pfx"];

fn is_sensitive_path(path: &Path) -> bool {
    path.components().any(|component| {
        let std::path::Component::Normal(name) = component else {
            return false;
        };
        let Some(name) = name.to_str() else {
            return true;
        };
        let name = name.to_ascii_lowercase();
        SENSITIVE_PATH_NAMES.contains(&name.as_str())
            || name.starts_with(".env.")
            || SENSITIVE_PATH_SIGNALS
                .iter()
                .any(|signal| name.contains(signal))
            || Path::new(&name)
                .extension()
                .and_then(|extension| extension.to_str())
                .is_some_and(|extension| SENSITIVE_PATH_EXTENSIONS.contains(&extension))
    })
}

/// Case-insensitive component patterns for the same credential paths
/// enforced by file tools. Execution adapters may enforce these at their filesystem boundary.
pub struct SensitiveWritePathPatterns {
    pub absolute_components: Vec<String>,
    pub relative_components: Vec<String>,
}

/// Escapes a literal path for regex engines without case-insensitive flags, including Seatbelt.
pub fn case_insensitive_path_pattern(path: &str) -> String {
    path.chars()
        .map(|character| {
            if character.is_ascii_alphabetic() {
                format!(
                    "[{}{}]",
                    character.to_ascii_lowercase(),
                    character.to_ascii_uppercase()
                )
            } else {
                regex::escape(&character.to_string())
            }
        })
        .collect()
}

pub fn credential_write_path_patterns() -> SensitiveWritePathPatterns {
    let absolute_components = SENSITIVE_PATH_NAMES
        .iter()
        .copied()
        .map(case_insensitive_path_pattern)
        .collect();
    let mut components = SENSITIVE_PATH_NAMES
        .iter()
        .map(|name| case_insensitive_path_pattern(name))
        .collect::<Vec<_>>();
    components.push(format!("{}[^/]*", case_insensitive_path_pattern(".env.")));
    components.extend(
        SENSITIVE_PATH_SIGNALS
            .iter()
            .map(|name| format!("[^/]*{}[^/]*", case_insensitive_path_pattern(name))),
    );
    components.extend(
        SENSITIVE_PATH_EXTENSIONS
            .iter()
            .map(|name| format!("[^/]*\\.{}", case_insensitive_path_pattern(name))),
    );
    SensitiveWritePathPatterns {
        absolute_components,
        relative_components: components,
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ToolError {
    #[error("invalid tool input: {0}")]
    InvalidInput(String),
    #[error("unknown tool: {0}")]
    UnknownTool(String),
    #[error("file or directory does not exist: {0}")]
    NotFound(PathBuf),
    #[error("unsupported file type: {0}")]
    UnsupportedType(PathBuf),
    #[error("file access failed for {path}: {reason}")]
    Io { path: PathBuf, reason: String },
    #[error("the path changed after the operation was prepared: {0}")]
    PathChanged(PathBuf),
    #[error("file changed or expected text does not match: {0}")]
    Conflict(PathBuf),
    #[error("network tool failed: {0}")]
    Network(String),
    #[error("the model request that applies the prompt to the fetched content failed: {0}")]
    SecondaryModel(String),
    /// A request the tool declines, explained to the model in Claude Code's words.
    #[error("{0}")]
    Rejected(String),
    #[error("cannot read image {path}: {source}")]
    Image { path: PathBuf, source: ImageError },
    #[error("tool execution was cancelled")]
    Cancelled,
    #[error(transparent)]
    FileChange(#[from] FileChangeArtifactError),
}

/// The local directories of one Chat with a Folder.
#[derive(Clone, Debug)]
pub struct ChatDirectory {
    /// File change reviews and stored tool results go here.
    pub chat: PrivateDirectory,
    /// Background task output directory; following Claude Code, `<chatId>/tasks` under the system
    /// temp directory.
    pub task_output: PrivateDirectory,
    /// Canonical private family temp space, shared by the family's root and its children.
    pub family_temp: PrivateDirectory,
}

#[derive(Clone)]
pub struct Tools {
    /// `None` for a Chat without a Folder, whose tools write no local file.
    directory: Option<ChatDirectory>,
    commands: shell::Registry,
    search: std::sync::Arc<tokio::sync::RwLock<Option<SearchConfig>>>,
    fetched_pages: web::FetchCache,
    /// What this Chat's fetches saw that may give provenance to later URLs.
    observed_urls: web_provenance::Observations,
    search_credential_root: Option<PathBuf>,
    search_provider_factory: Option<std::sync::Arc<dyn web_search::SearchProviderFactory>>,
    file_reads: FileReadState,
    /// The addresses `WebFetch` may reach.
    reach: Reach,
}

impl Tools {
    /// When used standalone, attachments and task output both go under `output_dir`; with no host
    /// to run commands, Shell commands are rejected.
    pub fn new(output_dir: PathBuf) -> Self {
        let output_dir = PrivateDirectory::trusted(output_dir);
        Self::with_commands(
            Some(ChatDirectory {
                task_output: output_dir.join(TASK_OUTPUT_SUBDIRECTORY),
                family_temp: output_dir.clone(),
                chat: output_dir,
            }),
            shell::Registry::without_host(),
            Reach::Anywhere,
        )
    }

    /// The tools of one Chat with a Folder, whose shell commands run in `shells`.
    pub fn for_chat(directory: ChatDirectory, shells: std::sync::Arc<dyn ShellHost>) -> Self {
        Self::with_commands(
            Some(directory),
            shell::Registry::new(shells),
            Reach::Anywhere,
        )
    }

    /// The tools of a Chat without a Folder: they touch no local file, run no command, and fetch
    /// only from the addresses `reach` admits.
    pub fn folderless(reach: Reach) -> Self {
        Self::with_commands(None, shell::Registry::without_folder(), reach)
    }

    fn with_commands(
        directory: Option<ChatDirectory>,
        commands: shell::Registry,
        reach: Reach,
    ) -> Self {
        Self {
            directory,
            commands,
            search: std::sync::Arc::new(tokio::sync::RwLock::new(SearchConfig::enabled_default())),
            fetched_pages: web::FetchCache::default(),
            observed_urls: web_provenance::Observations::default(),
            search_credential_root: None,
            search_provider_factory: None,
            file_reads: FileReadState::default(),
            reach,
        }
    }

    /// What this Chat's fetches saw since the last call: links on their pages' origins and
    /// redirects within a site, for the host to keep.
    pub fn take_fetch_observations(&self) -> web_provenance::FetchObservations {
        self.observed_urls.take()
    }

    pub fn with_search_configuration(
        mut self,
        config: Option<SearchConfig>,
        credential_root: Option<PathBuf>,
    ) -> Self {
        self.search = std::sync::Arc::new(tokio::sync::RwLock::new(config));
        self.search_credential_root = credential_root;
        self
    }

    pub async fn prepare_for_operation(
        &self,
        ids: &tickloop_ids::IdContext,
        workspace: &Workspace,
        name: &str,
        input: &Value,
        operation: uuid::Uuid,
    ) -> Result<PreparedTool, ToolError> {
        let mut prepared = self.prepare(ids, workspace, name, input).await?;
        match &mut prepared.operation {
            Operation::Shell(input) => {
                input.preparation = self
                    .commands
                    .prepare(
                        &prepared.resolved_path,
                        input,
                        self.task_output_directory()?,
                        operation,
                        workspace,
                        self.family_temp_directory(),
                    )
                    .await?;
            }
            Operation::TaskStop(input) => {
                input.preparation = self.commands.prepare_stop(ids, &input.task_id).await?;
            }
            _ => {}
        }
        Ok(prepared)
    }

    pub fn with_search_provider_factory(
        mut self,
        factory: Option<std::sync::Arc<dyn web_search::SearchProviderFactory>>,
    ) -> Self {
        self.search_provider_factory = factory;
        self
    }

    pub async fn prepare(
        &self,
        ids: &tickloop_ids::IdContext,
        workspace: &Workspace,
        name: &str,
        input: &Value,
    ) -> Result<PreparedTool, ToolError> {
        let Some(directory) = self.directory.clone() else {
            let prepared = prepare_with_readable_directories(
                workspace,
                name,
                input,
                &[],
                None,
                &self.file_reads,
            )?;
            if let Operation::WebFetch(fetch) = &prepared.operation {
                web::check_reach(&fetch.url, self.reach)?;
            }
            return self.bind_search(prepared).await;
        };
        let owned_workspace = workspace.clone();
        let owned_name = name.to_owned();
        let owned_input = input.clone();
        let family_temp = directory.family_temp.clone();
        family_temp
            .create()
            .map_err(|error| io_error(family_temp.path(), error))?;
        let scratchpad = family_temp.join(SCRATCHPAD_SUBDIRECTORY);
        scratchpad
            .create()
            .map_err(|error| io_error(scratchpad.path(), error))?;
        let readable_directories = vec![
            directory.chat.path().to_path_buf(),
            family_temp.path().to_path_buf(),
        ];
        let file_reads = self.file_reads.clone();
        let output_dir = directory.chat;
        let join_error_path = output_dir.path().to_path_buf();
        let prepared = tokio::task::spawn_blocking(move || {
            let mut prepared = prepare_with_readable_directories(
                &owned_workspace,
                &owned_name,
                &owned_input,
                &readable_directories,
                Some(family_temp.path()),
                &file_reads,
            )?;
            if let Operation::Write(operation) = &mut prepared.operation {
                let review = write::take_review(operation);
                let review_directory = output_dir.join(ARTIFACT_DIRECTORY);
                review_directory
                    .create()
                    .map_err(|error| FileChangeArtifactError::Io {
                        path: review_directory.path().to_path_buf(),
                        reason: error.to_string(),
                    })?;
                prepared.file_change =
                    Some(persist_file_change_review(output_dir.path(), &review)?);
            }
            Ok::<_, ToolError>(prepared)
        })
        .await
        .map_err(|error| ToolError::Io {
            path: join_error_path,
            reason: format!("tool preparation did not finish: {error}"),
        })??;
        if let Operation::TaskStop(input) = &prepared.operation {
            self.commands.stoppable_task(ids, &input.task_id).await?;
        }
        self.bind_search(prepared).await
    }

    /// Binds a `WebSearch` call to the Chat's search service and its credential before
    /// approval, with or without a Folder.
    async fn bind_search(&self, mut prepared: PreparedTool) -> Result<PreparedTool, ToolError> {
        if let Operation::WebSearch(search) = &mut prepared.operation {
            let config = self
                .search
                .read()
                .await
                .clone()
                .ok_or_else(|| ToolError::Network(web_search::DISABLED_MESSAGE.into()))?;
            let request = search.request();
            web_search::validate_request(&config, &request)
                .map_err(|error| ToolError::Network(error.to_string()))?;
            let plan = match &self.search_provider_factory {
                Some(factory) => tokio::time::timeout(
                    web_search::SEARCH_TIMEOUT,
                    factory.prepare(&config, &request),
                )
                .await
                .unwrap_or(Err(web_search::SearchError::Timeout)),
                None => {
                    web_search::prepare(&config, &request, self.search_credential_root.as_deref())
                }
            }
            .map_err(|error| ToolError::Network(error.to_string()))?;
            if plan.kind != config.provider {
                return Err(ToolError::Network(
                    "search provider does not match configuration".into(),
                ));
            }
            prepared.input["provider"] = json!(config.provider);
            prepared.input["endpoint"] = json!(plan.endpoints.first());
            prepared.input["endpoints"] = json!(plan.endpoints);
            search.prepared = Some(plan);
        }
        Ok(prepared)
    }

    /// Read-only access to this Chat's local files in `workspace`, for a host tool that sends a
    /// file elsewhere; `admission` decides each `Read` before the file is opened. A Chat without
    /// a Folder has none.
    pub fn local_file_reader(
        &self,
        workspace: &Workspace,
        admission: std::sync::Arc<dyn LocalReadAdmission>,
    ) -> Option<LocalFileReader> {
        let directory = self.directory.as_ref()?;
        Some(LocalFileReader {
            workspace: workspace.clone(),
            readable_directories: vec![
                directory.chat.path().to_path_buf(),
                directory.family_temp.path().to_path_buf(),
            ],
            family_temp: directory.family_temp.path().to_path_buf(),
            admission,
        })
    }

    pub async fn set_search(&self, config: Option<SearchConfig>) -> Result<(), ToolError> {
        if let Some(config) = &config {
            web::validate_search(config)?;
        }
        *self.search.write().await = config;
        Ok(())
    }

    pub async fn commands(&self) -> Vec<CommandSnapshot> {
        self.commands.snapshots().await
    }
    /// A busy command returns `None`; unsupported ownership transfer remains an error.
    pub async fn checkpoint_shells(&self) -> Result<Option<Value>, ToolError> {
        self.commands.checkpoint().await
    }
    pub async fn restore_shells(&self, checkpoint: Value) -> Result<(), ToolError> {
        self.commands.restore(checkpoint).await
    }
    pub async fn background(&self, id: uuid::Uuid) -> Result<CommandSnapshot, ToolError> {
        self.commands.background(id).await
    }
    pub async fn stop_command(&self, id: uuid::Uuid) -> Result<CommandSnapshot, ToolError> {
        self.commands.stop(id).await
    }
    pub async fn read_command(
        &self,
        id: uuid::Uuid,
        offset: u64,
        limit: usize,
    ) -> Result<ToolResult, ToolError> {
        self.commands.read(id, offset, limit).await
    }
    /// A live command's state, or `None` once this host no longer runs it.
    pub async fn command_state(&self, id: uuid::Uuid) -> Option<CommandState> {
        self.commands.state(id).await
    }

    /// Whether the model stopped this command with `TaskStop`, which already told it the outcome.
    pub async fn is_command_stopped_by_model(&self, id: uuid::Uuid) -> bool {
        self.commands.is_stopped_by_model(id).await
    }

    /// What a finished live command reports to the model.
    pub async fn finished_command_result(&self, id: uuid::Uuid) -> Option<ToolResult> {
        self.commands.finished_result(id).await
    }

    pub fn read_archived_command(
        command: &CommandSnapshot,
        offset: u64,
        limit: usize,
    ) -> Result<ToolResult, ToolError> {
        shell::command_output(command, offset, limit)
    }
    pub fn family_temp_directory(&self) -> Option<&Path> {
        Some(self.directory.as_ref()?.family_temp.path())
    }

    /// Where this Chat keeps file change reviews and stored tool results; `None` without a Folder.
    pub fn chat_directory(&self) -> Option<&PrivateDirectory> {
        Some(&self.directory.as_ref()?.chat)
    }

    /// What this Chat's `Read` calls returned, for Claude Code's read-before-write checks.
    pub fn file_reads(&self) -> &FileReadState {
        &self.file_reads
    }

    fn task_output_directory(&self) -> Result<&PrivateDirectory, ToolError> {
        self.directory
            .as_ref()
            .map(|directory| &directory.task_output)
            .ok_or_else(|| ToolError::Rejected(FOLDERLESS_REJECTION.into()))
    }

    pub fn subscribe_commands(&self) -> tokio::sync::broadcast::Receiver<CommandSnapshot> {
        self.commands.subscribe()
    }

    pub async fn execute_user_shell(
        &self,
        prepared: PreparedTool,
        operation_id: uuid::Uuid,
        chat_id: uuid::Uuid,
        cancel: CancellationToken,
    ) -> Result<ToolResult, ToolError> {
        let current = prepared
            .operation
            .resolve_target(&prepared.original_path, prepared.is_read_only)?;
        if current != prepared.resolved_path {
            return Err(ToolError::PathChanged(prepared.original_path));
        }
        prepared.revalidate_automatic_shell_paths()?;
        let Operation::Shell(input) = prepared.operation else {
            return Err(ToolError::InvalidInput(
                "user Shell needs a prepared Bash operation".into(),
            ));
        };
        self.commands
            .execute_user(
                &prepared.resolved_path,
                input,
                self.task_output_directory()?,
                operation_id,
                chat_id,
                cancel,
            )
            .await
    }

    pub async fn user_commands(&self) -> Vec<UserCommandSnapshot> {
        self.commands.user_snapshots().await
    }

    pub async fn background_user_shell(
        &self,
        operation_id: uuid::Uuid,
        chat_id: uuid::Uuid,
    ) -> Result<UserCommandSnapshot, ToolError> {
        self.commands.background_user(operation_id, chat_id).await
    }

    pub async fn stop_user_shell(
        &self,
        operation_id: uuid::Uuid,
        chat_id: uuid::Uuid,
    ) -> Result<UserCommandSnapshot, ToolError> {
        self.commands.stop_user(operation_id, chat_id).await
    }

    pub async fn read_user_shell(
        &self,
        operation_id: uuid::Uuid,
        chat_id: uuid::Uuid,
        offset: u64,
        limit: usize,
    ) -> Result<ToolResult, ToolError> {
        self.commands
            .read_user(operation_id, chat_id, offset, limit)
            .await
    }

    pub fn subscribe_user_shells(&self) -> tokio::sync::broadcast::Receiver<UserCommandSnapshot> {
        self.commands.subscribe_user()
    }

    pub fn read_archived_user_shell(
        command: &UserCommandSnapshot,
        offset: u64,
        limit: usize,
    ) -> Result<ToolResult, ToolError> {
        shell::user_command_output(command, offset, limit)
    }

    pub fn definitions() -> Vec<ToolDefinition> {
        builtin_registrations()
            .into_iter()
            .map(|registration| registration.definition)
            .collect()
    }

    /// Runs a prepared call. `WebFetch` asks `secondary_model` about the page it fetched.
    pub async fn execute_with_context(
        &self,
        prepared: PreparedTool,
        cancel: CancellationToken,
        context: ExecutionContext,
        secondary_model: &dyn SecondaryModel,
    ) -> ToolExecution {
        if let Operation::WebSearch(input) = &prepared.operation {
            return web::search(input.clone(), cancel)
                .await
                .unwrap_or_else(|error| Err(error).into());
        }
        if let Operation::WebFetch(input) = &prepared.operation {
            return web::fetch(
                input.clone(),
                &self.fetched_pages,
                &self.observed_urls,
                self.reach,
                secondary_model,
                cancel,
            )
            .await
            .unwrap_or_else(|error| Err(error).into());
        }
        if let Operation::TaskStop(input) = &prepared.operation {
            if cancel.is_cancelled() {
                return Err(ToolError::Cancelled).into();
            }
            return self
                .commands
                .stop_prepared_task(
                    &context.ids,
                    &input.task_id,
                    input.preparation.clone(),
                    cancel,
                )
                .await
                .into();
        }
        if let Operation::Shell(input) = &prepared.operation {
            return async {
                prepared.revalidate_automatic_shell_paths()?;
                if cancel.is_cancelled() {
                    return Err(ToolError::Cancelled);
                }
                if resolve_path(&prepared.original_path, false)? != prepared.resolved_path {
                    return Err(ToolError::PathChanged(prepared.original_path.clone()));
                }
                self.commands
                    .execute(
                        &prepared.resolved_path,
                        input.clone(),
                        self.task_output_directory()?,
                        &context.ids,
                        context.operation_id,
                        context.loop_id,
                        cancel,
                    )
                    .await
            }
            .await
            .into();
        }
        self.execute_file(prepared, cancel, context).await
    }

    #[cfg(test)]
    pub(crate) async fn execute(
        &self,
        prepared: PreparedTool,
        cancel: CancellationToken,
    ) -> Result<ToolResult, ToolError> {
        struct NoSecondaryModel;
        #[async_trait::async_trait]
        impl SecondaryModel for NoSecondaryModel {
            async fn respond(&self, _: String, _: CancellationToken) -> Result<String, ToolError> {
                Err(ToolError::SecondaryModel(
                    "tool tests have no secondary model".into(),
                ))
            }
        }
        let context = ExecutionContext {
            ids: tickloop_ids::IdContext::new(),
            operation_id: tickloop_ids::generate(),
            loop_id: uuid::Uuid::nil(),
            accepts_images: false,
        };
        self.execute_with_context(prepared, cancel, context, &NoSecondaryModel)
            .await
            .result
    }

    async fn execute_file(
        &self,
        prepared: PreparedTool,
        cancel: CancellationToken,
        context: ExecutionContext,
    ) -> ToolExecution {
        if cancel.is_cancelled() {
            return Err(ToolError::Cancelled).into();
        }
        let is_read_only = prepared.is_read_only;
        let resolved_path = prepared.resolved_path.clone();
        let execution_cancel = cancel.clone();
        let execution = tokio::task::spawn_blocking(move || {
            let current = prepared
                .operation
                .resolve_target(&prepared.original_path, is_read_only)?;
            if current != prepared.resolved_path {
                return Err(ToolError::PathChanged(prepared.original_path));
            }
            match prepared.operation {
                Operation::Shell(_)
                | Operation::TaskStop(_)
                | Operation::WebFetch(_)
                | Operation::WebSearch(_) => Err(ToolError::InvalidInput(
                    "shell requires asynchronous execution".into(),
                )),
                Operation::Write(operation) => write::execute(
                    &prepared.resolved_path,
                    *operation,
                    context.operation_id,
                    &execution_cancel,
                )
                .map(FileOutcome::from),
                Operation::Read(request) => {
                    read::execute(&prepared.resolved_path, &request, context.accepts_images)
                }
                Operation::Search(request) => {
                    search::execute(&prepared.resolved_path, &request, &execution_cancel)
                        .map(FileOutcome::from)
                }
            }
        });
        let joined = if !is_read_only {
            execution.await
        } else {
            tokio::select! {
                biased;
                _ = cancel.cancelled() => return Err(ToolError::Cancelled).into(),
                result = execution => result,
            }
        };
        let outcome = joined.unwrap_or_else(|_| {
            Err(ToolError::Io {
                path: self.chat_directory().map_or_else(
                    || resolved_path.clone(),
                    |directory| directory.path().into(),
                ),
                reason: "file task did not finish".into(),
            })
        });
        match outcome {
            Ok(outcome) => {
                if let Some(read) = outcome.read {
                    self.file_reads.record(resolved_path, read);
                }
                ToolExecution {
                    result: Ok(outcome.result),
                    images: outcome.images,
                    web_sources: Vec::new(),
                }
            }
            Err(error) => Err(error).into(),
        }
    }
}

/// A finished file operation, what the model saw of the file's lines, and any images it views.
struct FileOutcome {
    result: ToolResult,
    read: Option<FileReadRecord>,
    images: Vec<Image>,
}

impl From<ToolResult> for FileOutcome {
    fn from(result: ToolResult) -> Self {
        Self {
            result,
            read: None,
            images: Vec::new(),
        }
    }
}

impl From<(ToolResult, FileReadRecord)> for FileOutcome {
    fn from((result, read): (ToolResult, FileReadRecord)) -> Self {
        Self {
            result,
            read: Some(read),
            images: Vec::new(),
        }
    }
}

/// Prepares a call outside any Chat: nothing has been read, so `Write` and `Edit` only accept
/// files that do not exist yet.
pub fn is_concurrency_safe(workspace: &Workspace, name: &str, input: &Value) -> bool {
    match name {
        READ_TOOL_NAME | GLOB_TOOL_NAME | GREP_TOOL_NAME | WEB_FETCH_TOOL_NAME
        | WEB_SEARCH_TOOL_NAME => true,
        BASH_TOOL_NAME => shell::is_read_only_input(workspace, input),
        _ => false,
    }
}

pub fn prepare(
    workspace: &Workspace,
    name: &str,
    input: &Value,
) -> Result<PreparedTool, ToolError> {
    prepare_with_readable_directories(workspace, name, input, &[], None, &FileReadState::default())
}

type PrepareBuiltin = fn(&str, &Value) -> Result<(String, Operation, Value), ToolError>;

struct BuiltinRegistration {
    definition: ToolDefinition,
    prepare: PrepareBuiltin,
}

fn register_builtin(definition: ToolDefinition, prepare: PrepareBuiltin) -> BuiltinRegistration {
    BuiltinRegistration {
        definition,
        prepare,
    }
}

fn builtin_registrations() -> Vec<BuiltinRegistration> {
    let mut registrations = vec![register_builtin(read::definition(), read::prepare_input)];
    registrations.extend(
        search::definitions()
            .into_iter()
            .map(|definition| register_builtin(definition, search::prepare_input)),
    );
    registrations.extend(
        write::definitions()
            .into_iter()
            .map(|definition| register_builtin(definition, write::prepare_input)),
    );
    registrations.push(register_builtin(shell::definition(), |_, input| {
        shell::prepare_input(input)
    }));
    registrations.push(register_builtin(
        shell::task_stop_definition(),
        |_, input| shell::prepare_task_stop(input),
    ));
    registrations.push(register_builtin(web::fetch_definition(), |_, input| {
        web::prepare_fetch(input)
    }));
    registrations.push(register_builtin(web::search_definition(), |_, input| {
        web::prepare_search(input)
    }));
    registrations
}

fn prepare_with_readable_directories(
    workspace: &Workspace,
    name: &str,
    input: &Value,
    readable_directories: &[PathBuf],
    family_temp: Option<&Path>,
    file_reads: &FileReadState,
) -> Result<PreparedTool, ToolError> {
    let registration = builtin_registrations()
        .into_iter()
        .find(|registration| registration.definition.name == name)
        .ok_or_else(|| ToolError::UnknownTool(name.into()))?;
    let (path, mut operation, mut normalized_input) = (registration.prepare)(name, input)?;
    if path.is_empty() || path.contains('\0') {
        return Err(ToolError::InvalidInput(
            "path must not be empty or contain NUL".into(),
        ));
    }
    let Some(folder) = workspace.folder.as_deref() else {
        if !matches!(operation, Operation::WebFetch(_) | Operation::WebSearch(_)) {
            return Err(ToolError::Rejected(FOLDERLESS_REJECTION.into()));
        }
        return Ok(PreparedTool {
            name: name.into(),
            input: normalized_input,
            requires_permission: true,
            is_read_only: true,
            in_workspace: false,
            file_change: None,
            in_sibling_worktree: false,
            in_family_temp: false,
            automatic_shell_workspace: None,
            original_path: PathBuf::new(),
            resolved_path: PathBuf::new(),
            operation,
        });
    };
    let original_path = folder.join(&path);
    if let Operation::Search(request) = &mut operation {
        request.bind(&original_path, &path, folder)?;
    }
    let mut automatic_shell_workspace = None;
    let is_read_only = match &mut operation {
        Operation::Write(_) => false,
        Operation::Shell(input) => {
            input.readable_directories = readable_directories.to_vec();
            input.editable_directory = family_temp.map(Path::to_path_buf);
            let cwd = resolve_path(&original_path, true).ok();
            if let Some(cwd) = &cwd
                && input.is_allowed_with_family_temp(cwd, workspace)
            {
                automatic_shell_workspace = Some(workspace.clone());
            }
            input.is_read_only(cwd.as_deref().unwrap_or(&original_path), workspace)
        }
        Operation::TaskStop(_) => false,
        _ => true,
    };
    let resolved_path = operation.resolve_target(&original_path, is_read_only)?;
    if let Operation::Shell(input) = &mut operation {
        input.approved_directory = std::fs::metadata(&resolved_path)
            .ok()
            .and_then(|metadata| shell::DirectoryIdentity::of(&metadata));
    }
    let original_scope = match (original_path.parent(), original_path.file_name()) {
        (Some(parent), Some(name)) if matches!(operation, Operation::Write(_)) => {
            resolve_write_target(parent)?.join(name)
        }
        (Some(parent), Some(name)) => parent
            .canonicalize()
            .map_err(|error| io_error(parent, error))?
            .join(name),
        _ => resolved_path.clone(),
    };
    let operation = match operation {
        Operation::Write(operation) => Operation::Write(Box::new(write::bind(
            &resolved_path,
            *operation,
            file_reads,
            folder,
        )?)),
        other => other,
    };
    let is_local = !matches!(operation, Operation::WebFetch(_) | Operation::WebSearch(_));
    let in_workspace =
        is_local && workspace.contains(&original_scope) && workspace.contains(&resolved_path);
    let in_sibling_worktree = is_local
        && !in_workspace
        && workspace
            .with_sibling_worktrees()
            .is_some_and(|scope| scope.contains(&original_scope) && scope.contains(&resolved_path));
    let is_file_operation = matches!(
        operation,
        Operation::Read(_) | Operation::Write(_) | Operation::Search(_)
    );
    let in_family_temp = is_file_operation
        && family_temp.is_some_and(|root| {
            original_scope.starts_with(root) && resolved_path.starts_with(root)
        });
    let is_internal_read = is_read_only
        && is_file_operation
        && readable_directories
            .iter()
            .any(|root| original_scope.starts_with(root) && resolved_path.starts_with(root));
    let requires_permission = operation.has_permission_check()
        && !in_family_temp
        && automatic_shell_workspace.is_none()
        && !is_internal_read
        && (!in_workspace || !is_read_only);
    if let Some(path_field) = operation.path_field() {
        normalized_input[path_field] = json!(resolved_path);
    }
    Ok(PreparedTool {
        name: name.into(),
        input: normalized_input,
        requires_permission,
        is_read_only,
        in_workspace,
        file_change: None,
        in_sibling_worktree,
        in_family_temp,
        automatic_shell_workspace,
        original_path,
        resolved_path,
        operation,
    })
}

/// Opens an existing path without following symlinks; with `directory`, the last component must
/// also be a directory.
pub(crate) fn open_existing(path: &Path, directory: bool) -> Result<File, ToolError> {
    let target = if directory {
        OpenTarget::Directory { create_mode: None }
    } else {
        OpenTarget::Entry
    };
    open_without_following_links(path, target).map_err(|error| io_error(path, error))
}

fn read_whole_file(path: &Path, max_bytes: usize) -> Result<Vec<u8>, ToolError> {
    let file = open_existing(path, false)?;
    let metadata = file.metadata().map_err(|error| io_error(path, error))?;
    if metadata.is_dir() {
        return Err(ToolError::Rejected(format!(
            "EISDIR: illegal operation on a directory, read '{}'",
            path.display()
        )));
    }
    if !metadata.is_file() {
        return Err(ToolError::UnsupportedType(path.into()));
    }
    let too_large = |size: usize| {
        ToolError::Rejected(format!(
            "File content ({}) exceeds maximum allowed size ({}).",
            format_file_size(size),
            format_file_size(max_bytes)
        ))
    };
    let size = usize::try_from(metadata.len()).unwrap_or(usize::MAX);
    if size > max_bytes {
        return Err(too_large(size));
    }
    let mut bytes = Vec::with_capacity(size);
    file.take(max_bytes as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| io_error(path, error))?;
    if bytes.len() > max_bytes {
        return Err(too_large(bytes.len()));
    }
    Ok(bytes)
}

/// Claude Code `formatFileSize`.
pub fn format_file_size(size: usize) -> String {
    let kilobytes = size as f64 / 1024.0;
    if kilobytes < 1.0 {
        return format!("{size} bytes");
    }
    let megabytes = kilobytes / 1024.0;
    let gigabytes = megabytes / 1024.0;
    let (value, unit) = if kilobytes < 1024.0 {
        (kilobytes, "KB")
    } else if megabytes < 1024.0 {
        (megabytes, "MB")
    } else {
        (gigabytes, "GB")
    };
    let text = format!("{value:.1}");
    format!("{}{unit}", text.strip_suffix(".0").unwrap_or(&text))
}

fn io_error(path: &Path, error: std::io::Error) -> ToolError {
    if error.kind() == std::io::ErrorKind::NotFound {
        ToolError::NotFound(path.into())
    } else {
        ToolError::Io {
            path: path.into(),
            reason: error.to_string(),
        }
    }
}

/// Resolves a file to write: the deepest existing ancestor is canonicalized and the missing names
/// below it are appended unchanged. A missing part may not climb with `..`, and a dangling link
/// anywhere on the way counts as a changed path.
fn resolve_write_target(path: &Path) -> Result<PathBuf, ToolError> {
    let mut missing_names = Vec::new();
    let mut candidate = path;
    loop {
        match candidate.canonicalize() {
            Ok(existing) => {
                return Ok(missing_names
                    .iter()
                    .rev()
                    .fold(existing, |resolved, name| resolved.join(name)));
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(io_error(candidate, error)),
        }
        if std::fs::symlink_metadata(candidate).is_ok() {
            return Err(ToolError::PathChanged(path.into()));
        }
        let Some(std::path::Component::Normal(name)) = candidate.components().next_back() else {
            return Err(ToolError::InvalidInput(format!(
                "{} must name a file below existing directories without `..`",
                path.display()
            )));
        };
        missing_names.push(name);
        candidate = candidate
            .parent()
            .ok_or_else(|| ToolError::InvalidInput("path has no parent".into()))?;
    }
}

fn resolve_path(path: &Path, allow_missing: bool) -> Result<PathBuf, ToolError> {
    match path.canonicalize() {
        Ok(resolved) => Ok(resolved),
        Err(error) if allow_missing && error.kind() == std::io::ErrorKind::NotFound => {
            if std::fs::symlink_metadata(path).is_ok() {
                return Err(ToolError::PathChanged(path.into()));
            }
            let parent = path
                .parent()
                .ok_or_else(|| ToolError::InvalidInput("path has no parent".into()))?;
            let parent = parent
                .canonicalize()
                .map_err(|error| io_error(parent, error))?;
            let name = path
                .file_name()
                .ok_or_else(|| ToolError::InvalidInput("path has no file name".into()))?;
            Ok(parent.join(name))
        }
        Err(error) => Err(io_error(path, error)),
    }
}

#[cfg(test)]
mod security_tests {
    use super::*;

    #[test]
    fn sensitive_paths_cover_hidden_and_common_credential_shapes() {
        for path in [
            ".netrc",
            ".npmrc",
            ".kube/config",
            ".docker/config.json",
            "Library/Keychains/login.keychain-db",
            "deploy-token.txt",
            "service-credentials.json",
            "certificate.pem",
        ] {
            assert!(
                is_sensitive_path(Path::new(path)),
                "sensitive path was not classified: {path}"
            );
        }
        assert!(!is_sensitive_path(Path::new("src/runtime.rs")));
    }
}
