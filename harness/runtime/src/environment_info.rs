//! Claude Code's `# Environment` section and its git status snapshot: what a Chat with a Folder
//! learns about where it runs. The environment is read once per Chat, at its first conversation
//! request, so the system prompt stays the same for the rest of the Chat; the git status joins the
//! user context, read again only when that is.

use std::{path::Path, process::Command};

/// Claude Code's `MAX_STATUS_CHARS`.
const MAX_STATUS_CHARS: usize = 2_000;
const RECENT_COMMITS: &str = "5";

/// What a Chat read about its Folder and system when it first asked the model.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct ChatEnvironment {
    is_git: bool,
    os_version: Option<String>,
}

impl ChatEnvironment {
    /// Reads the environment of `folder`; it runs `uname` and read-only `git` commands, so call it
    /// on a blocking thread.
    pub(crate) fn capture(folder: &Path) -> Self {
        let os_version = output(Command::new("uname").arg("-sr"));
        Self {
            is_git: is_git(folder),
            os_version,
        }
    }

    /// The `# Environment` section for a Chat working in `folder` with `model`, followed by the
    /// git status snapshot in a repository.
    pub(crate) fn section(
        &self,
        folder: &Path,
        model: &tickloop_harness_protocol::ModelConfig,
    ) -> String {
        let model_line = match &model.card {
            Some(card) => format!(
                "You are powered by the model named {}. The exact model ID is {}.",
                card.display_name, model.model
            ),
            None => format!("You are powered by the model {}.", model.model),
        };
        let mut lines = vec![
            format!("Primary working directory: {}", folder.display()),
            format!("Is a git repository: {}", self.is_git),
            format!("Platform: {}", std::env::consts::OS),
        ];
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

    #[test]
    fn a_folder_outside_git_reports_no_repository_and_no_status() {
        let directory = tempfile::tempdir().unwrap();
        let environment = ChatEnvironment::capture(directory.path());
        assert!(!environment.is_git);
        assert_eq!(git_status(directory.path()), None);
        let section = environment.section(directory.path(), &fixture_model());
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
        let environment = ChatEnvironment::capture(directory.path());
        assert!(environment.is_git);
        let status = git_status(directory.path()).unwrap();
        assert!(status.contains("Current branch: trunk"), "{status}");
        assert!(status.contains("Status:\n?? new.txt"), "{status}");
        assert!(status.contains("first commit"), "{status}");
        let section = environment.section(directory.path(), &fixture_model());
        assert!(section.contains("Is a git repository: true"));
        assert!(status.starts_with("This is the git status at the start of the conversation."));
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
