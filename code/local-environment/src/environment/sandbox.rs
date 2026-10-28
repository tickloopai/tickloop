use regex::RegexSet;
use std::{
    collections::{HashMap, VecDeque},
    path::{Component, Path, PathBuf},
};
use tickhost::sandbox::{ProtectedPathPattern, SandboxNetwork, SandboxPolicy};
use tickloop_code_settings::{UserSettings, sandbox::SandboxSettings};
use tickloop_harness::runtime::environment::{
    ExecutionError, ExecutionIsolation, NetworkIsolation, ShellSpec, WriteIsolation,
};

const MAX_SYMLINK_HOPS: usize = 40;
const MAX_POLICY_ENTRIES: usize = 1_000_000;

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
    let config = settings.load().sandbox().map_err(configuration_error)?;
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
    let settings = settings.clone();
    let spec = spec.clone();
    let policy = tokio::task::spawn_blocking(move || prepare_policy(&settings, &spec, &config))
        .await
        .map_err(|error| configuration_error(error.to_string()))??;
    Ok(PreparedSandbox {
        policy: Some(policy),
        isolation: ExecutionIsolation {
            writes: WriteIsolation::FolderAndDirectories,
            network: NetworkIsolation::Disconnected,
        },
        warning: None,
    })
}

fn prepare_policy(
    settings: &UserSettings,
    spec: &ShellSpec,
    config: &SandboxSettings,
) -> Result<SandboxPolicy, ExecutionError> {
    let folder = Path::new(&spec.cwd).canonicalize().map_err(path_error)?;
    let mut writable_roots = vec![folder.clone()];
    for path in &spec.additional_directories {
        writable_roots.push(Path::new(path).canonicalize().map_err(path_error)?);
    }
    writable_roots.extend(tickloop_harness::base::git_worktree::sibling_linked_worktrees(&folder));
    let mut protected_paths = Vec::new();
    for path in &config.filesystem.allow_write {
        let (resolved, anchors) =
            resolve_configured_path(path, &folder, settings.home_directory())?;
        writable_roots.push(resolved);
        protected_paths.extend(anchors);
    }
    if let Some(path) = &spec.family_temp {
        writable_roots.push(Path::new(path).canonicalize().map_err(path_error)?);
    }
    writable_roots.push(std::env::temp_dir().canonicalize().map_err(path_error)?);
    let mut seen = std::collections::HashSet::new();
    writable_roots.retain(|root| seen.insert(root.clone()));
    let mut deny_write = Vec::new();
    for path in &config.filesystem.deny_write {
        let (resolved, anchors) =
            resolve_configured_path(path, &folder, settings.home_directory())?;
        deny_write.push(resolved);
        protected_paths.extend(anchors);
    }
    if let Some(root) = settings.root() {
        deny_write.push(root.canonicalize().map_err(path_error)?);
    }
    let (resolved, anchors) = resolve_path(settings.file_path())?;
    deny_write.push(resolved);
    protected_paths.extend(anchors);

    let patterns = tickloop_harness::tools::sensitive_write_path_patterns();
    let absolute = RegexSet::new(
        patterns
            .absolute_components
            .iter()
            .map(|component| format!("(^|/){component}(/|$)")),
    )
    .map_err(|error| configuration_error(error.to_string()))?;
    let relative = RegexSet::new(
        patterns
            .relative_components
            .iter()
            .map(|component| format!("(^|/){component}(/|$)")),
    )
    .map_err(|error| configuration_error(error.to_string()))?;
    protect_sensitive_aliases(
        &writable_roots,
        &absolute,
        &relative,
        &mut deny_write,
        &mut protected_paths,
    )?;
    let mut denied_path_patterns = absolute
        .patterns()
        .iter()
        .map(|pattern| ProtectedPathPattern {
            pattern: pattern.clone(),
            excluded_roots: vec![],
        })
        .collect::<Vec<_>>();
    for (index, root) in writable_roots.iter().enumerate() {
        if writable_roots[..index]
            .iter()
            .any(|earlier| root.starts_with(earlier))
        {
            continue;
        }
        let prefix = root
            .to_str()
            .ok_or_else(|| configuration_error("unsupported writable path"))?;
        let excluded_roots = writable_roots[..index]
            .iter()
            .filter(|earlier| earlier.starts_with(root))
            .cloned()
            .collect::<Vec<_>>();
        for component in &patterns.relative_components {
            denied_path_patterns.push(ProtectedPathPattern {
                pattern: format!(
                    "^{}/(.*/)?{component}(/|$)",
                    regex::escape(prefix.trim_end_matches('/'))
                ),
                excluded_roots: excluded_roots.clone(),
            });
        }
    }
    deny_write.sort();
    deny_write.dedup();
    protected_paths.sort();
    protected_paths.dedup();
    Ok(SandboxPolicy {
        writable_roots,
        deny_write,
        protected_paths,
        denied_path_patterns,
        network: SandboxNetwork::Disconnected,
    })
}

fn protect_sensitive_aliases(
    roots: &[PathBuf],
    absolute: &RegexSet,
    relative: &RegexSet,
    deny_write: &mut Vec<PathBuf>,
    protected_paths: &mut Vec<PathBuf>,
) -> Result<(), ExecutionError> {
    let mut pending = roots
        .iter()
        .cloned()
        .map(|root| (root, false))
        .collect::<Vec<_>>();
    let mut visited = HashMap::new();
    let mut entries = 0;
    while let Some((directory, inherited_sensitive)) = pending.pop() {
        if visited
            .get(&directory)
            .is_some_and(|sensitive| *sensitive || !inherited_sensitive)
        {
            continue;
        }
        visited.insert(directory.clone(), inherited_sensitive);
        let children = match std::fs::read_dir(&directory) {
            Ok(children) => children,
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
                ) =>
            {
                continue;
            }
            Err(error) => {
                return Err(configuration_error(format!(
                    "cannot inspect sandbox directory {}: {error}",
                    directory.display()
                )));
            }
        };
        for child in children {
            entries += 1;
            if entries > MAX_POLICY_ENTRIES {
                return Err(configuration_error(
                    "sandbox path inspection limit exceeded",
                ));
            }
            let child = match child {
                Ok(child) => child,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => return Err(path_error(error)),
            };
            let path = child.path();
            let kind = match child.file_type() {
                Ok(kind) => kind,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => return Err(path_error(error)),
            };
            if !kind.is_dir() && !kind.is_symlink() {
                continue;
            }
            let below = roots
                .iter()
                .find_map(|root| path.strip_prefix(root).ok())
                .unwrap_or(&path);
            let sensitive = inherited_sensitive
                || absolute.is_match(&path.to_string_lossy())
                || relative.is_match(&below.to_string_lossy());
            if kind.is_dir() {
                pending.push((path, sensitive));
            } else if sensitive {
                let (resolved, anchors) = resolve_path(&path)?;
                protected_paths.extend(anchors);
                deny_write.push(resolved.clone());
                if resolved != Path::new("/") {
                    pending.push((resolved, true));
                }
            }
        }
    }
    Ok(())
}

fn resolve_configured_path(
    path: &str,
    folder: &Path,
    home: Option<&Path>,
) -> Result<(PathBuf, Vec<PathBuf>), ExecutionError> {
    let path = if let Some(relative) = path.strip_prefix("~/") {
        home.ok_or_else(|| configuration_error("home directory unavailable for sandbox path"))?
            .join(relative)
    } else {
        folder.join(path)
    };
    resolve_path(&path)
}

fn resolve_path(path: &Path) -> Result<(PathBuf, Vec<PathBuf>), ExecutionError> {
    if !path.is_absolute() {
        return Err(configuration_error("sandbox path must be absolute"));
    }
    let mut pending = path
        .components()
        .map(|component| component.as_os_str().to_owned())
        .collect::<VecDeque<_>>();
    let mut resolved = PathBuf::new();
    let mut anchors = Vec::new();
    let mut hops = 0;
    while let Some(part) = pending.pop_front() {
        match Path::new(&part).components().next().unwrap() {
            Component::RootDir => {
                resolved = PathBuf::from("/");
                continue;
            }
            Component::CurDir => continue,
            Component::ParentDir => {
                resolved.pop();
                continue;
            }
            _ => {}
        }
        resolved.push(part);
        match std::fs::symlink_metadata(&resolved) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                hops += 1;
                if hops > MAX_SYMLINK_HOPS {
                    return Err(configuration_error("too many symlinks in sandbox path"));
                }
                anchors.push(resolved.clone());
                let target = std::fs::read_link(&resolved).map_err(path_error)?;
                resolved.pop();
                for component in target.components().rev() {
                    pending.push_front(component.as_os_str().to_owned());
                }
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(path_error(error)),
        }
    }
    Ok((resolved, anchors))
}

fn configuration_error(message: impl Into<String>) -> ExecutionError {
    ExecutionError::new("SandboxConfiguration", message)
}
fn path_error(error: std::io::Error) -> ExecutionError {
    configuration_error(error.to_string())
}
