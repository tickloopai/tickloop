//! Where the file tools of one Chat find a file: in the Chat's own directories on this machine, or
//! in its Folder and Additional Directories through the Chat's [`FileHost`].

use std::{
    path::{Component, Path, PathBuf},
    sync::Arc,
};

use crate::{
    ToolError,
    file_port::{
        ExecutionError, FileAccess, FileHost, FileInspection, FileKind, FilePath, FileQuery,
        ResolvedPath, error_code,
    },
    local_roots::LocalRoots,
};

#[derive(Clone)]
pub(crate) struct ChatFiles {
    environment: Arc<dyn FileHost>,
    /// The Chat's own directories, which the Harness keeps on this machine whatever the
    /// environment.
    harness: Option<Arc<LocalRoots>>,
}

/// A file a tool acts on, with symbolic links resolved, and the host that reaches it.
#[derive(Clone)]
pub(crate) struct FileTarget {
    pub(crate) host: Arc<dyn FileHost>,
    pub(crate) path: FilePath,
    /// The absolute path the model sees for it.
    pub(crate) shown: PathBuf,
    /// What was there when the path was resolved; `None` when nothing was.
    pub(crate) kind: Option<FileKind>,
    /// The path as the model wrote it, before its links were resolved, and its host.
    as_requested: (Arc<dyn FileHost>, FilePath),
}

impl std::fmt::Debug for FileTarget {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("FileTarget")
            .field("path", &self.path)
            .field("shown", &self.shown)
            .field("kind", &self.kind)
            .finish_non_exhaustive()
    }
}

/// A file of a Chat and the host that holds it, before its links are resolved.
#[derive(Clone)]
pub struct FileLocation {
    pub host: Arc<dyn FileHost>,
    pub path: FilePath,
}

impl FileLocation {
    /// The absolute path the model sees for the file.
    pub fn display(&self) -> String {
        self.host.capabilities().display(&self.path)
    }
}

impl std::fmt::Debug for FileLocation {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("FileLocation")
            .field("path", &self.path)
            .finish_non_exhaustive()
    }
}

/// Finds the files of one Chat by the absolute paths the model sees for them, where its file
/// tools find them: in its own directories on this machine, or through its environment.
#[derive(Clone)]
pub struct FileLocator {
    files: ChatFiles,
}

impl FileLocator {
    /// Files reached only through `environment`.
    pub fn new(environment: Arc<dyn FileHost>) -> Self {
        Self {
            files: ChatFiles::new(environment, None),
        }
    }

    pub(crate) fn for_chat(files: ChatFiles) -> Self {
        Self { files }
    }

    /// Where `path` is, without resolving its links; the Chat must be able to read there.
    pub fn locate(&self, path: &Path) -> Result<FileLocation, ToolError> {
        let placement = self.files.place(path)?;
        Ok(FileLocation {
            host: placement.host,
            path: placement.path,
        })
    }
}

/// A path below one host's roots, before its links are resolved.
struct Placement {
    host: Arc<dyn FileHost>,
    path: FilePath,
    in_harness_directory: bool,
}

impl ChatFiles {
    pub(crate) fn new(environment: Arc<dyn FileHost>, harness: Option<LocalRoots>) -> Self {
        Self {
            environment,
            harness: harness.map(Arc::new),
        }
    }

    /// Whether `path`, an absolute path as the model sees it, lies in the Folder or an
    /// Additional Directory.
    pub(crate) fn is_in_roots(&self, path: &Path) -> bool {
        path.to_str()
            .and_then(|path| self.environment.capabilities().resolve(path).ok())
            .is_some_and(|path| path.root().is_some())
    }

    /// `path` below the Folder or Additional Directory that holds it, or the whole path when none
    /// does.
    pub(crate) fn below_roots(&self, path: &Path) -> PathBuf {
        path.to_str()
            .and_then(|text| self.environment.capabilities().resolve(text).ok())
            .filter(|below| below.root().is_some())
            .map_or_else(|| path.to_path_buf(), |below| PathBuf::from(below.path()))
    }

    /// The absolute path the model sees for the Folder.
    pub(crate) fn folder_display(&self) -> Option<PathBuf> {
        self.environment
            .capabilities()
            .folder()
            .map(|folder| PathBuf::from(&folder.display_path))
    }

    /// Where the Folder is on this machine, when the environment says it is here.
    pub(crate) fn local_folder(&self) -> Option<PathBuf> {
        self.environment
            .capabilities()
            .folder()
            .and_then(|folder| folder.local_path.clone())
    }

    /// The file the absolute `requested` path leads to. A path in the Chat's own directories is
    /// resolved on this machine, any other through the environment, which must allow reading it;
    /// a symbolic link that leads out of the directories it starts in is followed to wherever its
    /// target is.
    pub(crate) async fn resolve(&self, requested: &Path) -> Result<FileTarget, ToolError> {
        let placement = self.place(requested)?;
        let as_requested = (placement.host.clone(), placement.path.clone());
        let resolved = resolve_on(placement.host.as_ref(), &placement.path, requested).await?;
        let real = PathBuf::from(placement.host.capabilities().display(&resolved.path));
        let left_roots = resolved.path.root().is_none();
        let placement = match (placement.in_harness_directory, left_roots) {
            (true, true) => self.place_in_environment(&real)?,
            // Only an environment on this machine can lead into the Chat's own directories.
            (false, true) if self.local_folder().is_some() => {
                self.place_in_harness_directory(&real).unwrap_or(Placement {
                    path: resolved.path,
                    ..placement
                })
            }
            _ => Placement {
                path: resolved.path,
                ..placement
            },
        };
        if !placement
            .host
            .capabilities()
            .access(&placement.path)
            .can_read()
        {
            return Err(outside_readable(&real));
        }
        Ok(FileTarget {
            host: placement.host,
            path: placement.path,
            shown: real,
            kind: resolved.kind,
            as_requested,
        })
    }

    /// `requested` with the links of its parent resolved and its own name kept, which tells a
    /// link from what it leads to; a parent the Chat cannot read, such as the directory holding
    /// an Additional Directory, stays as written.
    pub(crate) async fn resolve_scope(
        &self,
        requested: &Path,
        target: &FileTarget,
    ) -> Result<PathBuf, ToolError> {
        let (Some(parent), Some(name)) = (requested.parent(), requested.file_name()) else {
            return Ok(target.shown.clone());
        };
        match self.place(parent) {
            Ok(_) => Ok(self.resolve(parent).await?.shown.join(name)),
            Err(_) => Ok(requested.to_path_buf()),
        }
    }

    /// Refuses a `..` that would climb out of a directory that does not exist, which the file
    /// system refuses too, before the path is resolved by name.
    pub(crate) async fn check_parent_steps(&self, requested: &Path) -> Result<(), ToolError> {
        let mut walked = PathBuf::new();
        for component in requested.components() {
            if component == Component::ParentDir {
                let kind = self
                    .resolve(&walked)
                    .await
                    .ok()
                    .and_then(|target| target.kind);
                if kind != Some(FileKind::Directory) {
                    return Err(ToolError::InvalidInput(format!(
                        "{} must name a file below existing directories without `..`",
                        requested.display()
                    )));
                }
            }
            walked.push(component);
        }
        Ok(())
    }

    fn place(&self, requested: &Path) -> Result<Placement, ToolError> {
        match self.place_in_harness_directory(requested) {
            Some(placement) => Ok(placement),
            None => self.place_in_environment(requested),
        }
    }

    fn place_in_harness_directory(&self, requested: &Path) -> Option<Placement> {
        let harness = self.harness.as_ref()?;
        let path = harness
            .capabilities()
            .resolve(requested.to_str()?)
            .ok()
            .filter(|path| path.root().is_some())?;
        Some(Placement {
            host: harness.clone(),
            path,
            in_harness_directory: true,
        })
    }

    fn place_in_environment(&self, requested: &Path) -> Result<Placement, ToolError> {
        let capabilities = self.environment.capabilities();
        let path = requested
            .to_str()
            .ok_or_else(|| {
                ToolError::InvalidInput(format!("{} is not UTF-8 text", requested.display()))
            })
            .and_then(|text| {
                capabilities
                    .resolve(text)
                    .map_err(|error| ToolError::InvalidInput(error.message))
            })?;
        if !capabilities.access(&path).can_read() {
            return Err(outside_readable(Path::new(&capabilities.display(&path))));
        }
        Ok(Placement {
            host: self.environment.clone(),
            path,
            in_harness_directory: false,
        })
    }
}

fn outside_readable(path: &Path) -> ToolError {
    ToolError::Rejected(format!(
        "{} is outside the directories this Chat can read.",
        path.display()
    ))
}

impl FileTarget {
    pub(crate) fn location(&self) -> FileLocation {
        FileLocation {
            host: self.host.clone(),
            path: self.path.clone(),
        }
    }

    /// Refuses a change of a file in a directory the Chat may only read, before any approval.
    pub(crate) fn check_writable(&self, tool: &str) -> Result<(), ToolError> {
        if self.host.capabilities().access(&self.path) == FileAccess::ReadWrite {
            return Ok(());
        }
        Err(ToolError::Rejected(format!(
            "{} is in a directory this Chat can only read, so {tool} cannot change it.",
            self.shown.display()
        )))
    }

    /// Refuses the file once the path the model wrote leads somewhere else than when it was
    /// resolved, such as to another file through a replaced symbolic link.
    pub(crate) async fn check_unchanged(&self, requested: &Path) -> Result<(), ToolError> {
        let (host, path) = &self.as_requested;
        let resolved = resolve_on(host.as_ref(), path, requested).await?;
        if Path::new(&host.capabilities().display(&resolved.path)) != self.shown {
            return Err(ToolError::PathChanged(requested.into()));
        }
        Ok(())
    }

    pub(crate) async fn inspect(&self, query: FileQuery) -> Result<FileInspection, ToolError> {
        self.host
            .inspect(query)
            .await
            .map_err(|error| tool_error(error, &self.shown))
    }
}

async fn resolve_on(
    host: &dyn FileHost,
    path: &FilePath,
    requested: &Path,
) -> Result<ResolvedPath, ToolError> {
    match host.inspect(FileQuery::Resolve(path.clone())).await {
        Ok(FileInspection::Resolved(resolved)) => Ok(resolved),
        Ok(_) => Err(ToolError::Io {
            path: requested.into(),
            reason: "the environment answered a path resolution with something else".into(),
        }),
        Err(error) => Err(tool_error(error, requested)),
    }
}

/// The tool error for an environment's answer about `path`.
pub(crate) fn tool_error(error: ExecutionError, path: &Path) -> ToolError {
    match error.code.as_str() {
        error_code::NOT_FOUND => ToolError::NotFound(path.into()),
        error_code::OUTSIDE_ROOT => ToolError::PathChanged(path.into()),
        error_code::CONFLICT => ToolError::Conflict(path.into()),
        error_code::IS_DIRECTORY | error_code::UNSUPPORTED => {
            ToolError::UnsupportedType(path.into())
        }
        error_code::INVALID_INPUT => ToolError::InvalidInput(error.message),
        error_code::CANCELLED => ToolError::Cancelled,
        error_code::NOT_A_DIRECTORY | error_code::TOO_LARGE | error_code::ACCESS_DENIED => {
            ToolError::Rejected(error.message)
        }
        _ => ToolError::Io {
            path: path.into(),
            reason: error.message,
        },
    }
}
