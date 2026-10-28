use std::{
    collections::VecDeque,
    path::{Component, Path, PathBuf},
};
use tickhost::sandbox::{ProtectedPathPattern, SandboxNetwork, SandboxPolicy};
use tickloop_code_settings::{UserSettings, sandbox::SandboxSettings};
use tickloop_harness::runtime::environment::{
    ExecutionError, ExecutionIsolation, NetworkIsolation, ShellSpec, WriteIsolation,
};

const MAX_SYMLINK_HOPS: usize = 40;

#[derive(Default)]
pub(super) struct PreparedSandbox {
    pub policy: Option<SandboxPolicy>,
    pub isolation: ExecutionIsolation,
    pub warning: Option<String>,
    pub requires_approval: bool,
    pub skips_approval_when_isolated: bool,
    pub escape_guidance: bool,
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
    if (config.allow_unsandboxed_commands && spec.dangerously_disable_sandbox)
        || config.excluded_commands.iter().any(|pattern| {
            tickloop_harness::tools::permissions::shell_command_matches_pattern(
                pattern,
                &spec.command,
            )
        })
    {
        return Ok(PreparedSandbox {
            requires_approval: true,
            ..Default::default()
        });
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
    let escape_guidance = config.allow_unsandboxed_commands;
    let skips_approval_when_isolated = config.auto_allow_bash_if_sandboxed;
    let settings = settings.clone();
    let spec = spec.clone();
    let (policy, requires_approval) =
        tokio::task::spawn_blocking(move || prepare_policy(&settings, &spec, &config))
            .await
            .map_err(|error| configuration_error(error.to_string()))??;
    let network = match &policy.network {
        SandboxNetwork::Disconnected => NetworkIsolation::Disconnected,
        SandboxNetwork::Allowlist { .. } => NetworkIsolation::Allowlist,
    };
    Ok(PreparedSandbox {
        policy: Some(policy),
        isolation: ExecutionIsolation {
            writes: WriteIsolation::FolderAndDirectories,
            network,
        },
        warning: None,
        requires_approval,
        skips_approval_when_isolated,
        escape_guidance,
    })
}

fn prepare_policy(
    settings: &UserSettings,
    spec: &ShellSpec,
    config: &SandboxSettings,
) -> Result<(SandboxPolicy, bool), ExecutionError> {
    let folder = spec
        .folder
        .as_deref()
        .ok_or_else(|| configuration_error("sandbox requires a trusted Chat Folder"))?;
    let folder = Path::new(folder).canonicalize().map_err(path_error)?;
    let mut writable_roots = vec![folder.clone()];
    for path in &spec.additional_directories {
        writable_roots.push(Path::new(path).canonicalize().map_err(path_error)?);
    }
    let cwd = Path::new(&spec.cwd).canonicalize().map_err(path_error)?;
    let requires_approval = !writable_roots.iter().any(|root| cwd.starts_with(root));
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

    let mut patterns = tickloop_harness::tools::credential_write_path_patterns();
    for path in config.filesystem.default_deny_write_paths() {
        patterns
            .absolute_components
            .push(tickloop_harness::tools::case_insensitive_path_pattern(path));
        // Protect the container itself against replacement or symlink traversal, while its
        // ordinary children (notably Git objects, index and refs) remain writable.
        for (separator, _) in path.match_indices('/') {
            patterns.absolute_components.push(format!(
                "{}$",
                tickloop_harness::tools::case_insensitive_path_pattern(&path[..separator])
            ));
        }
    }
    patterns.absolute_components.sort();
    patterns.absolute_components.dedup();
    let mut denied_path_patterns = patterns
        .absolute_components
        .iter()
        .map(|component| ProtectedPathPattern {
            pattern: format!("(^|/){component}(/|$)"),
            excluded_roots: vec![],
        })
        .collect::<Vec<_>>();
    let pattern_roots = writable_roots
        .iter()
        .map(|root| {
            if root.is_dir() {
                root.clone()
            } else {
                root.parent().unwrap_or(root).to_path_buf()
            }
        })
        .collect::<Vec<_>>();
    for component in &patterns.relative_components {
        denied_path_patterns.push(ProtectedPathPattern {
            pattern: format!("(^|/){component}(/|$)"),
            excluded_roots: pattern_roots.clone(),
        });
    }
    for (index, root) in pattern_roots.iter().enumerate() {
        if pattern_roots[..index]
            .iter()
            .any(|earlier| root.starts_with(earlier))
        {
            continue;
        }
        let prefix = root
            .to_str()
            .ok_or_else(|| configuration_error("unsupported writable path"))?;
        let excluded_roots = pattern_roots[..index]
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
    Ok((
        SandboxPolicy {
            writable_roots,
            deny_write,
            protected_paths,
            denied_path_patterns,
            network: if config.network.allowed_domains.is_empty() {
                SandboxNetwork::Disconnected
            } else {
                SandboxNetwork::Allowlist {
                    allowed_domains: config.network.allowed_domains.clone(),
                    proxy_port: 0,
                }
            },
        },
        requires_approval,
    ))
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

/// Keep optional guidance in Code so a running 10.1 Tickhost receives its unchanged policy.
pub(super) fn annotate_escape_guidance(output: &std::fs::File) {
    use std::{io::Write, os::unix::fs::FileExt};
    let Ok(length) = output.metadata().map(|metadata| metadata.len()) else {
        return;
    };
    let note = tickhost::sandbox::DENIAL_NOTE;
    let mut bytes = vec![0; length.min((note.len() + 2) as u64) as usize];
    let offset = length.saturating_sub(bytes.len() as u64);
    let Ok(read) = output.read_at(&mut bytes, offset) else {
        return;
    };
    if String::from_utf8_lossy(&bytes[..read])
        .trim_end()
        .ends_with(note)
    {
        let _ = writeln!(
            &mut &*output,
            "可用 dangerouslyDisableSandbox: true 申请在沙箱外运行，仍须正常审批"
        );
    }
}
