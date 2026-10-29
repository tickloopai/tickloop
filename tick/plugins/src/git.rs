//! Noninteractive, bounded system Git checkouts. A checkout owns its temporary directory.

use std::{
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};

use tokio::process::Command;

use crate::{GitSource, PluginError};

pub(crate) const COMMIT_REVISION_LENGTH: usize = 12;
const REMOTE_BRANCHES: &str = "+refs/heads/*:refs/remotes/origin/*";
const REMOTE_TAGS: &str = "+refs/tags/*:refs/tags/*";
const GIT_TIMEOUT: Duration = Duration::from_secs(120);

#[derive(Clone, Debug)]
pub struct GitClient {
    executable: PathBuf,
    timeout: Duration,
}

impl Default for GitClient {
    fn default() -> Self {
        Self::new("git", GIT_TIMEOUT)
    }
}

pub struct GitCheckout {
    _directory: tempfile::TempDir,
    pub(crate) source: GitSource,
    plugin_root: PathBuf,
    pub(crate) commit: String,
}

impl GitCheckout {
    pub fn root(&self) -> &Path {
        &self.plugin_root
    }
    pub fn commit(&self) -> &str {
        &self.commit
    }
    pub fn source(&self) -> &GitSource {
        &self.source
    }
}

impl GitClient {
    /// Selects the system executable and per-command deadline; no shell is involved.
    pub fn new(executable: impl Into<PathBuf>, timeout: Duration) -> Self {
        Self {
            executable: executable.into(),
            timeout,
        }
    }

    pub async fn checkout(
        &self,
        source: &GitSource,
        parent: &Path,
    ) -> Result<GitCheckout, PluginError> {
        source.validate()?;
        let directory = tempfile::Builder::new()
            .prefix(".checkout-")
            .tempdir_in(parent)
            .map_err(PluginError::io("cannot stage Git checkout"))?;
        let destination = directory.path().to_string_lossy().into_owned();
        self.run(
            parent,
            &[
                "clone",
                "--depth",
                "1",
                "--no-checkout",
                "--end-of-options",
                &source.url,
                &destination,
            ],
        )
        .await?;
        let root = directory.path();
        let target = if let Some(sha) = &source.sha {
            let sha = sha.to_ascii_lowercase();
            let available = if sha.len() == 40 {
                self.run(
                    root,
                    &["fetch", "--depth", "1", "--end-of-options", "origin", &sha],
                )
                .await
                .is_ok()
            } else {
                self.run(
                    root,
                    &[
                        "rev-parse",
                        "--verify",
                        "--end-of-options",
                        &format!("{sha}^{{commit}}"),
                    ],
                )
                .await
                .is_ok()
            };
            if !available {
                // Some servers refuse object-id fetches; abbreviated ids also need local history.
                let mut arguments = vec![
                    "fetch",
                    "--unshallow",
                    "--no-tags",
                    "--end-of-options",
                    "origin",
                ];
                if let Some(reference) = source.reference.as_deref() {
                    arguments.push(reference);
                } else {
                    arguments.extend([REMOTE_BRANCHES, REMOTE_TAGS]);
                }
                self.run(root, &arguments).await?;
            }
            sha
        } else if let Some(reference) = source.reference.as_deref() {
            self.run(
                root,
                &[
                    "fetch",
                    "--depth",
                    "1",
                    "--end-of-options",
                    "origin",
                    reference,
                ],
            )
            .await?;
            "FETCH_HEAD".to_owned()
        } else {
            "HEAD".to_owned()
        };
        let commit = self
            .run(
                root,
                &[
                    "rev-parse",
                    "--verify",
                    "--end-of-options",
                    &format!("{target}^{{commit}}"),
                ],
            )
            .await
            .map_err(|error| {
                PluginError::Invalid(format!("cannot resolve Git commit {target}: {error}"))
            })?;
        if !valid_sha(&commit) {
            return Err(PluginError::Invalid(
                "Git returned an invalid commit SHA".into(),
            ));
        }
        if source
            .sha
            .as_ref()
            .is_some_and(|sha| !commit.starts_with(&sha.to_ascii_lowercase()))
        {
            return Err(PluginError::Invalid(format!(
                "Git commit SHA mismatch: expected {}, got {commit}",
                source.sha.as_deref().unwrap_or_default()
            )));
        }
        if let Some(path) = &source.subdirectory {
            self.run(root, &["sparse-checkout", "init", "--cone"])
                .await?;
            self.run(
                root,
                &[
                    "sparse-checkout",
                    "set",
                    "--end-of-options",
                    path.strip_prefix("./").unwrap_or(path),
                ],
            )
            .await?;
        }
        self.run(root, &["checkout", "--detach", "--end-of-options", &commit])
            .await?;
        let plugin_root = match &source.subdirectory {
            Some(path) => contained_directory(root, path)?,
            None => root.to_path_buf(),
        };
        Ok(GitCheckout {
            plugin_root,
            _directory: directory,
            source: source.clone(),
            commit,
        })
    }

    async fn run(&self, directory: &Path, args: &[&str]) -> Result<String, PluginError> {
        let mut command = Command::new(&self.executable);
        command
            .args([
                "-c",
                "credential.interactive=false",
                "-c",
                "core.hooksPath=/dev/null",
            ])
            .args(args)
            .current_dir(directory)
            .stdin(Stdio::null())
            .kill_on_drop(true)
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_ALLOW_PROTOCOL", "https:ssh:file")
            .env("GIT_ASKPASS", "/usr/bin/false")
            .env("SSH_ASKPASS", "/usr/bin/false")
            .env("GIT_SSH_COMMAND", "ssh -oBatchMode=yes");
        for variable in [
            "GIT_DIR",
            "GIT_WORK_TREE",
            "GIT_INDEX_FILE",
            "GIT_OBJECT_DIRECTORY",
            "GIT_ALTERNATE_OBJECT_DIRECTORIES",
        ] {
            command.env_remove(variable);
        }
        command.stdout(Stdio::piped()).stderr(Stdio::piped());
        command.process_group(0);
        let child = command.spawn().map_err(|error| {
            if error.kind() == std::io::ErrorKind::NotFound {
                PluginError::Invalid(
                    "Git is not installed; install git to use Git plugin sources".into(),
                )
            } else {
                PluginError::io("cannot run Git")(error)
            }
        })?;
        let _group = ProcessGroup(child.id().expect("spawned Git has a process id"));
        let output = tokio::time::timeout(self.timeout, child.wait_with_output())
            .await
            .map_err(|_| PluginError::Invalid("Git operation timed out".into()))?
            .map_err(PluginError::io("cannot wait for Git"))?;
        if !output.status.success() {
            return Err(PluginError::Invalid(format!(
                "Git operation failed: {}",
                crate::printable(String::from_utf8_lossy(&output.stderr).trim())
            )));
        }
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
    }
}

fn valid_sha(sha: &str) -> bool {
    matches!(sha.len(), 40 | 64) && sha.bytes().all(|byte| byte.is_ascii_hexdigit())
}

impl GitSource {
    pub(crate) fn validate(&self) -> Result<(), PluginError> {
        if !(self.url.starts_with("https://")
            || self.url.starts_with("ssh://")
            || self.url.starts_with("file://")
            || self.url.starts_with("git@"))
            || self.url.chars().any(char::is_control)
        {
            return Err(PluginError::Invalid(
                "Git source requires an https, ssh or file URL".into(),
            ));
        }
        if self.reference.as_ref().is_some_and(|reference| {
            reference.is_empty()
                || reference.starts_with('-')
                || !reference
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || b"._/-".contains(&byte))
        }) {
            return Err(PluginError::Invalid("invalid Git ref".into()));
        }
        if self.sha.as_ref().is_some_and(|sha| {
            !(7..=40).contains(&sha.len()) || !sha.bytes().all(|byte| byte.is_ascii_hexdigit())
        }) {
            return Err(PluginError::Invalid(
                "Git SHA must contain 7 to 40 hexadecimal characters".into(),
            ));
        }
        if let Some(path) = &self.subdirectory {
            validate_relative_directory(path)?;
        }
        Ok(())
    }
}

pub(crate) fn validate_relative_directory(path: &str) -> Result<(), PluginError> {
    use std::path::Component;
    if path.is_empty()
        || path.chars().any(char::is_control)
        || path.starts_with('-')
        || Path::new(path)
            .components()
            .any(|component| match component {
                Component::Normal(name) => name == ".git",
                Component::CurDir => false,
                _ => true,
            })
    {
        return Err(PluginError::Invalid(format!(
            "invalid plugin subdirectory {path:?}"
        )));
    }
    Ok(())
}

pub(crate) fn contained_directory(root: &Path, relative: &str) -> Result<PathBuf, PluginError> {
    validate_relative_directory(relative)?;
    let root = root
        .canonicalize()
        .map_err(PluginError::io("cannot open source directory"))?;
    let directory = root
        .join(relative)
        .canonicalize()
        .map_err(PluginError::io("cannot open plugin subdirectory"))?;
    if !directory.starts_with(&root) || !directory.is_dir() {
        return Err(PluginError::Invalid(
            "plugin subdirectory escapes its source directory or is not a directory".into(),
        ));
    }
    Ok(directory)
}

struct ProcessGroup(u32);

impl Drop for ProcessGroup {
    fn drop(&mut self) {
        // Git starts SSH and credential helpers; cancellation must also stop those descendants.
        unsafe {
            libc::kill(-(self.0 as libc::pid_t), libc::SIGKILL);
        }
    }
}
