//! Claude Code's `# Environment` section and its git status snapshot: what a Chat with a Folder
//! learns about where it runs. The environment is read once per Chat, at its first conversation
//! request, so the system prompt stays the same for the rest of the Chat; the git status joins the
//! user context, read again only when that is. Only a Folder on this machine is read: for a Folder
//! elsewhere no `uname` or `git` runs, and the section leaves out what they would tell.

use std::{path::Path, process::Command};

use crate::environment::{FileAccess, FileCapabilities};

/// Claude Code's `MAX_STATUS_CHARS`.
const MAX_STATUS_CHARS: usize = 2_000;
const RECENT_COMMITS: &str = "5";
const READ_ONLY_MARK: &str = " (read-only)";
const NO_FILE_ACCESS_MARK: &str = " (no file access)";

/// What a Chat read about its Folder and system when it first asked the model.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct ChatEnvironment {
    /// `None` for a Folder that is not on this machine.
    is_git: Option<bool>,
    os_version: Option<String>,
}

impl ChatEnvironment {
    /// Reads the environment of the Folder at `local_folder` on this machine; it runs `uname` and
    /// read-only `git` commands there, so call it on a blocking thread. A Folder elsewhere
    /// (`None`) runs nothing.
    pub(crate) fn capture(local_folder: Option<&Path>) -> Self {
        let Some(folder) = local_folder else {
            return Self::default();
        };
        let os_version = output(Command::new("uname").arg("-sr"));
        Self {
            is_git: Some(is_git(folder)),
            os_version,
        }
    }

    /// The `# Environment` section for a Chat working in `directories` with `model`.
    pub(crate) fn section(
        &self,
        directories: &WorkingDirectories,
        model: &tickloop_harness_protocol::ModelConfig,
    ) -> String {
        let model_line = match &model.card {
            Some(card) => format!(
                "You are powered by the model named {}. The exact model ID is {}.",
                card.display_name, model.model
            ),
            None => format!("You are powered by the model {}.", model.model),
        };
        let mut lines = vec![format!(
            "Primary working directory: {}{}",
            directories.folder, directories.folder_mark
        )];
        if let Some(is_git) = self.is_git {
            lines.push(format!("Is a git repository: {is_git}"));
        }
        if !directories.additional.is_empty() {
            lines.push(format!(
                "Additional working directories: {}",
                directories.additional.join(", ")
            ));
        }
        lines.push(format!("Platform: {}", std::env::consts::OS));
        if let Some(os_version) = &self.os_version {
            lines.push(format!("OS Version: {os_version}"));
        }
        lines.push(model_line);
        format!(
            "# Environment\nYou have been invoked in the following environment:\n{}",
            lines
                .iter()
                .map(|line| format!(" - {line}"))
                .collect::<Vec<_>>()
                .join("\n")
        )
    }
}

/// The Folder and Additional Directories as the system prompt names them: by the display paths
/// their Port gives, marking those the file tools cannot write. An Additional Directory on this
/// machine that the file tools can write is reached by its own path and goes unlisted, as it did
/// before Ports offered files.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct WorkingDirectories {
    pub(crate) folder: String,
    folder_mark: &'static str,
    additional: Vec<String>,
}

impl WorkingDirectories {
    /// `folder` names the Folder when the Port offers no root for it.
    pub(crate) fn new(folder: &Path, files: &FileCapabilities) -> Self {
        let (folder, folder_mark) = match files.folder() {
            Some(root) => (root.display_path.clone(), access_mark(root.access)),
            None => (folder.display().to_string(), ""),
        };
        let additional = files
            .roots()
            .iter()
            .filter(|root| {
                !root.is_folder
                    && (root.local_path.is_none() || root.access != FileAccess::ReadWrite)
            })
            .map(|root| format!("{}{}", root.display_path, access_mark(root.access)))
            .collect();
        Self {
            folder,
            folder_mark,
            additional,
        }
    }
}

fn access_mark(access: FileAccess) -> &'static str {
    match access {
        FileAccess::ReadWrite => "",
        FileAccess::Read => READ_ONLY_MARK,
        FileAccess::None => NO_FILE_ACCESS_MARK,
    }
}

fn git(folder: &Path) -> Command {
    let mut command = Command::new("git");
    command.current_dir(folder).args(["--no-optional-locks"]);
    command
}

/// The trimmed standard output of a command that succeeded.
fn output(command: &mut Command) -> Option<String> {
    let output = command.stdin(std::process::Stdio::null()).output().ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

fn is_git(folder: &Path) -> bool {
    output(git(folder).args(["rev-parse", "--is-inside-work-tree"]))
        .is_some_and(|inside| inside == "true")
}

/// Claude Code's `getGitStatus`: `None` outside a git repository. It runs read-only `git`
/// commands, so call it on a blocking thread.
pub(crate) fn git_status(folder: &Path) -> Option<String> {
    if !is_git(folder) {
        return None;
    }
    let branch = output(git(folder).args(["branch", "--show-current"])).unwrap_or_default();
    let main_branch =
        output(git(folder).args(["symbolic-ref", "--short", "refs/remotes/origin/HEAD"]))
            .and_then(|reference| {
                reference
                    .split_once('/')
                    .map(|(_, branch)| branch.to_owned())
            })
            .unwrap_or_else(|| "main".to_owned());
    let status = output(git(folder).args(["status", "--short"])).unwrap_or_default();
    let log =
        output(git(folder).args(["log", "--oneline", "-n", RECENT_COMMITS])).unwrap_or_default();
    let user_name =
        output(git(folder).args(["config", "user.name"])).filter(|name| !name.is_empty());
    let status = if status.chars().count() > MAX_STATUS_CHARS {
        format!(
            "{}\n... (truncated because it exceeds 2k characters. If you need more information, run \"git status\" using Bash)",
            status.chars().take(MAX_STATUS_CHARS).collect::<String>()
        )
    } else if status.is_empty() {
        "(clean)".to_owned()
    } else {
        status
    };
    let mut parts = vec![
        "This is the git status at the start of the conversation. Note that this status is a snapshot in time, and will not update during the conversation.".to_owned(),
        format!("Current branch: {branch}"),
        format!("Main branch (you will usually use this for PRs): {main_branch}"),
    ];
    if let Some(user_name) = user_name {
        parts.push(format!("Git user: {user_name}"));
    }
    parts.push(format!("Status:\n{status}"));
    parts.push(format!("Recent commits:\n{log}"));
    Some(parts.join("\n\n"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::environment::{FileRoot, RootName};

    #[test]
    fn a_folder_outside_git_reports_no_repository_and_no_status() {
        let directory = tempfile::tempdir().unwrap();
        let environment = ChatEnvironment::capture(Some(directory.path()));
        assert_eq!(environment.is_git, Some(false));
        assert_eq!(git_status(directory.path()), None);
        let section = environment.section(&local_directories(directory.path()), &fixture_model());
        assert!(section.starts_with("# Environment\n"));
        assert!(section.contains("Is a git repository: false"));
        assert!(section.contains("You are powered by the model fixture-model."));
    }

    #[test]
    fn a_repository_reports_its_branch_status_and_commits() {
        let directory = tempfile::tempdir().unwrap();
        let run = |args: &[&str]| {
            assert!(
                Command::new("git")
                    .args(args)
                    .current_dir(directory.path())
                    .env("GIT_CONFIG_GLOBAL", "/dev/null")
                    .status()
                    .unwrap()
                    .success()
            );
        };
        run(&["init", "-q", "-b", "trunk"]);
        run(&[
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=f@example.test",
            "commit",
            "-q",
            "--allow-empty",
            "-m",
            "first commit",
        ]);
        std::fs::write(directory.path().join("new.txt"), "x").unwrap();
        let environment = ChatEnvironment::capture(Some(directory.path()));
        assert_eq!(environment.is_git, Some(true));
        let status = git_status(directory.path()).unwrap();
        assert!(status.contains("Current branch: trunk"), "{status}");
        assert!(status.contains("Status:\n?? new.txt"), "{status}");
        assert!(status.contains("first commit"), "{status}");
        let section = environment.section(&local_directories(directory.path()), &fixture_model());
        assert!(section.contains("Is a git repository: true"));
        assert!(status.starts_with("This is the git status at the start of the conversation."));
    }

    /// Local roots are writable and shown by their own paths, so a local Chat's section reads as
    /// it did before Ports offered files, whether or not the Port reports its roots.
    #[test]
    fn a_local_folder_with_writable_directories_keeps_the_section_unchanged() {
        let environment = ChatEnvironment {
            is_git: Some(true),
            os_version: Some("Darwin 25.0.0".into()),
        };
        let folder = Path::new("/work/project");
        let expected = format!(
            "# Environment\nYou have been invoked in the following environment:\n - Primary working directory: /work/project\n - Is a git repository: true\n - Platform: {}\n - OS Version: Darwin 25.0.0\n - You are powered by the model fixture-model.",
            std::env::consts::OS
        );
        for files in [FileCapabilities::default(), local_files()] {
            let directories = WorkingDirectories::new(folder, &files);
            assert_eq!(directories.folder, "/work/project");
            assert_eq!(
                environment.section(&directories, &fixture_model()),
                expected
            );
        }
    }

    #[test]
    fn a_folder_elsewhere_runs_nothing_and_lists_its_directories_with_their_access() {
        let environment = ChatEnvironment::capture(None);
        assert_eq!(environment, ChatEnvironment::default());
        let files = FileCapabilities::new(
            vec![
                root("design", "/design", FileAccess::Read, true, None),
                root("browser", "/browser", FileAccess::ReadWrite, false, None),
                root("vault", "/vault", FileAccess::None, false, None),
                root(
                    "notes",
                    "/work/notes",
                    FileAccess::Read,
                    false,
                    Some("/work/notes"),
                ),
            ],
            FileAccess::None,
        )
        .unwrap();
        let section = environment.section(
            &WorkingDirectories::new(Path::new("/srv/folder"), &files),
            &fixture_model(),
        );
        assert_eq!(
            section,
            format!(
                "# Environment\nYou have been invoked in the following environment:\n - Primary working directory: /design (read-only)\n - Additional working directories: /browser, /vault (no file access), /work/notes (read-only)\n - Platform: {}\n - You are powered by the model fixture-model.",
                std::env::consts::OS
            )
        );
    }

    fn local_directories(folder: &Path) -> WorkingDirectories {
        WorkingDirectories::new(folder, &FileCapabilities::default())
    }

    fn local_files() -> FileCapabilities {
        FileCapabilities::new(
            vec![
                root(
                    "project",
                    "/work/project",
                    FileAccess::ReadWrite,
                    true,
                    Some("/work/project"),
                ),
                root(
                    "shared",
                    "/work/shared",
                    FileAccess::ReadWrite,
                    false,
                    Some("/work/shared"),
                ),
            ],
            FileAccess::ReadWrite,
        )
        .unwrap()
    }

    fn root(
        name: &str,
        display_path: &str,
        access: FileAccess,
        is_folder: bool,
        local_path: Option<&str>,
    ) -> FileRoot {
        FileRoot {
            name: RootName::new(name).unwrap(),
            display_path: display_path.into(),
            access,
            is_folder,
            local_path: local_path.map(Into::into),
        }
    }

    fn fixture_model() -> tickloop_harness_protocol::ModelConfig {
        serde_json::from_value(serde_json::json!({
            "name": "fixture",
            "protocol": "messages",
            "base_url": "http://127.0.0.1",
            "model": "fixture-model",
            "api_key_env": "",
            "max_output_tokens": 1024,
        }))
        .unwrap()
    }
}
