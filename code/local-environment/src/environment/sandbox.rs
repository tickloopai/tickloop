use std::path::{Path, PathBuf};
use tickhost::sandbox::{SandboxNetwork, SandboxPolicy};
use tickloop_code_settings::UserSettings;
use tickloop_harness::runtime::environment::{
    ExecutionError, ExecutionIsolation, NetworkIsolation, ShellSpec, WriteIsolation,
};

#[derive(Default)]
pub(super) struct PreparedSandbox {
    pub policy: Option<SandboxPolicy>,
    pub isolation: ExecutionIsolation,
    pub warning: Option<String>,
}

pub(super) async fn prepare(
    settings: Option<&UserSettings>,
    spec: &ShellSpec,
) -> Result<PreparedSandbox, ExecutionError> {
    let Some(settings) = settings else {
        return Ok(PreparedSandbox::default());
    };
    let config = settings
        .load()
        .sandbox()
        .map_err(|error| ExecutionError::new("SandboxConfiguration", error))?;
    if !config.enabled {
        return Ok(PreparedSandbox::default());
    }
    if let Err(reason) = tickhost::sandbox::availability().await {
        if config.fail_if_unavailable {
            return Err(ExecutionError::new("SandboxUnavailable", reason));
        }
        return Ok(PreparedSandbox {
            warning: Some(format!("{reason}; running without isolation")),
            ..Default::default()
        });
    }
    let folder = Path::new(&spec.cwd).canonicalize().map_err(path_error)?;
    let mut writable_roots = vec![
        folder.clone(),
        std::env::temp_dir().canonicalize().map_err(path_error)?,
        PathBuf::from("/private/tmp"),
    ];
    for path in spec
        .additional_directories
        .iter()
        .chain(spec.family_temp.iter())
    {
        writable_roots.push(Path::new(path).canonicalize().map_err(path_error)?);
    }
    writable_roots.extend(tickloop_harness::base::git_worktree::sibling_linked_worktrees(&folder));
    let mut protected_paths = Vec::new();
    for path in &config.filesystem.allow_write {
        let (resolved, anchors) = resolve(path, &folder, settings.home_directory())?;
        writable_roots.push(resolved);
        protected_paths.extend(anchors);
    }
    let mut deny_write = Vec::new();
    for path in &config.filesystem.deny_write {
        let (resolved, anchors) = resolve(path, &folder, settings.home_directory())?;
        deny_write.push(resolved);
        protected_paths.extend(anchors);
    }
    if let Some(root) = settings.root() {
        deny_write.push(root.canonicalize().map_err(path_error)?);
    }
    let settings_path = settings
        .file_path()
        .to_str()
        .ok_or_else(|| ExecutionError::new("SandboxConfiguration", "unsupported settings path"))?;
    let (resolved, anchors) = resolve(settings_path, &folder, settings.home_directory())?;
    deny_write.push(resolved);
    protected_paths.extend(anchors);
    protected_paths.sort();
    protected_paths.dedup();
    writable_roots.sort();
    writable_roots.dedup();
    Ok(PreparedSandbox {
        policy: Some(SandboxPolicy {
            writable_roots,
            deny_write,
            protected_paths,
            denied_path_patterns: tickloop_harness::tools::sensitive_write_path_patterns(),
            network: SandboxNetwork::Disconnected,
        }),
        isolation: ExecutionIsolation {
            writes: WriteIsolation::FolderAndDirectories,
            network: NetworkIsolation::Disconnected,
        },
        warning: None,
    })
}

fn resolve(
    path: &str,
    folder: &Path,
    home: Option<&Path>,
) -> Result<(PathBuf, Vec<PathBuf>), ExecutionError> {
    let path = if let Some(relative) = path.strip_prefix("~/") {
        home.ok_or_else(|| {
            ExecutionError::new(
                "SandboxConfiguration",
                "home directory unavailable for sandbox path",
            )
        })?
        .join(relative)
    } else {
        folder.join(path)
    };
    let mut protected_paths = Vec::new();
    for prefix in path.ancestors() {
        if std::fs::symlink_metadata(prefix).is_ok_and(|metadata| metadata.file_type().is_symlink())
        {
            let parent = prefix.parent().ok_or_else(|| {
                ExecutionError::new("SandboxConfiguration", "invalid symlink path")
            })?;
            let name = prefix.file_name().ok_or_else(|| {
                ExecutionError::new("SandboxConfiguration", "invalid symlink name")
            })?;
            protected_paths.push(parent.canonicalize().map_err(path_error)?.join(name));
        }
    }
    // Resolve existing symlinks even when the final component does not exist yet.
    let mut suffix = Vec::new();
    let mut ancestor = path.as_path();
    while !ancestor.exists() {
        suffix.push(
            ancestor.file_name().ok_or_else(|| {
                ExecutionError::new("SandboxConfiguration", "invalid sandbox path")
            })?,
        );
        ancestor = ancestor
            .parent()
            .ok_or_else(|| ExecutionError::new("SandboxConfiguration", "invalid sandbox path"))?;
    }
    let mut resolved = ancestor.canonicalize().map_err(path_error)?;
    for name in suffix.into_iter().rev() {
        resolved.push(name);
    }
    Ok((resolved, protected_paths))
}

fn path_error(error: std::io::Error) -> ExecutionError {
    ExecutionError::new("SandboxConfiguration", error.to_string())
}
