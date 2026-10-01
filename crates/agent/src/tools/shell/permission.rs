//! Port of the rule-free decision flow of Claude Code's `bashToolHasPermission`
//! (`tools/BashTool/bashPermissions.ts`), including pipe segmentation from
//! `tools/BashTool/bashCommandHelpers.ts`, the Accept Edits command set from
//! `tools/BashTool/modeValidation.ts` and `checkSedConstraints` from
//! `tools/BashTool/sedValidation.ts`. External Claude Code builds evaluate commands with the
//! legacy shell-quote path (`TREE_SITTER_BASH` is ant-only), so that path's compound-command
//! rules are the reference here.

use std::path::{Path, PathBuf};

use super::{
    path_constraints::PathPolicy,
    read_only::is_read_only_subcommand,
    security::{Safety, bash_command_is_safe, is_normalized_cd_command, is_normalized_git_command},
    sed::sed_command_is_allowed,
    syntax::{Separator, SimpleCommand, join_commands, parse},
};

const ACCEPT_EDITS_ALLOWED_COMMANDS: &[&str] =
    &["mkdir", "touch", "rm", "rmdir", "mv", "cp", "sed"];
const MAX_SUBCOMMANDS_FOR_SECURITY_CHECK: usize = 50;

pub(super) struct ShellPermissionContext<'a> {
    pub(super) cwd: &'a Path,
    pub(super) working_directories: Vec<PathBuf>,
    pub(super) readable_directories: &'a [PathBuf],
}

/// Whether Claude Code would allow `command` without asking when no permission rule matches.
/// `accept_edits` selects Accept Edits mode; otherwise the default/auto checks apply.
pub(super) fn is_allowed_without_rules(
    command: &str,
    context: &ShellPermissionContext,
    accept_edits: bool,
) -> bool {
    let Some(commands) = parse(command) else {
        return false;
    };
    let policy = PathPolicy {
        cwd: context.cwd,
        working_directories: &context.working_directories,
        readable_directories: context.readable_directories,
        accept_edits,
    };
    Evaluation { context, policy }.command_allowed(command, &commands)
}

struct Evaluation<'a> {
    context: &'a ShellPermissionContext<'a>,
    policy: PathPolicy<'a>,
}

impl Evaluation<'_> {
    fn command_allowed(&self, text: &str, commands: &[SimpleCommand]) -> bool {
        let segments = commands
            .split_inclusive(|command| command.separator == Some(Separator::Pipe))
            .collect::<Vec<_>>();
        if segments.len() > 1 {
            self.pipeline_allowed(text, commands, &segments)
        } else {
            self.command_list_allowed(text, commands)
        }
    }

    /// `segmentedCommandPermissionResult` plus the whole-command checks that follow an allow.
    fn pipeline_allowed(
        &self,
        text: &str,
        commands: &[SimpleCommand],
        segments: &[&[SimpleCommand]],
    ) -> bool {
        let segment_texts = segments
            .iter()
            .map(|segment| join_commands(segment))
            .collect::<Vec<_>>();
        let cd_segments = segment_texts
            .iter()
            .filter(|segment| is_normalized_cd_command(segment.trim()))
            .count();
        let has_cd = commands
            .iter()
            .any(|command| is_normalized_cd_command(&command.text));
        let has_git = commands
            .iter()
            .any(|command| is_normalized_git_command(&command.text));
        cd_segments <= 1
            && !(has_cd && has_git)
            && segments
                .iter()
                .zip(&segment_texts)
                .all(|(segment, segment_text)| self.command_allowed(segment_text, segment))
            && bash_command_is_safe(text) == Safety::Passthrough
            && self.whole_command_paths_allowed(commands, has_cd)
    }

    fn command_list_allowed(&self, text: &str, commands: &[SimpleCommand]) -> bool {
        if bash_command_is_safe(text) == (Safety::Ask { misparsing: true }) {
            return false;
        }
        let cd_to_cwd = format!("cd {}", self.context.cwd.display());
        let subcommands = commands
            .iter()
            .filter(|command| command.text != cd_to_cwd)
            .collect::<Vec<_>>();
        if subcommands.len() > MAX_SUBCOMMANDS_FOR_SECURITY_CHECK {
            return false;
        }
        let cd_count = subcommands
            .iter()
            .filter(|command| is_normalized_cd_command(&command.text))
            .count();
        let has_cd = cd_count == 1;
        let has_git = subcommands
            .iter()
            .any(|command| is_normalized_git_command(command.text.trim()));
        cd_count <= 1
            && !(has_cd && has_git)
            && subcommands
                .iter()
                .all(|command| self.subcommand_allowed(&command.text, has_cd))
            && self.whole_command_paths_allowed(commands, has_cd)
            && subcommands
                .iter()
                .all(|command| bash_command_is_safe(&command.text) == Safety::Passthrough)
    }

    /// `checkPathConstraints` on the original command: redirect targets and every path command.
    fn whole_command_paths_allowed(&self, commands: &[SimpleCommand], has_cd: bool) -> bool {
        let targets = commands
            .iter()
            .flat_map(|command| &command.redirects)
            .map(|redirect| redirect.target.as_str())
            .collect::<Vec<_>>();
        self.policy.output_redirections_allowed(&targets, has_cd)
            && commands
                .iter()
                .all(|command| self.policy.command_paths_allowed(&command.text, has_cd))
    }

    /// `bashToolCheckPermission` without rules.
    fn subcommand_allowed(&self, command: &str, compound_has_cd: bool) -> bool {
        let command = command.trim();
        if !self.policy.command_paths_allowed(command, compound_has_cd) {
            return false;
        }
        let base_command = command.split_whitespace().next().unwrap_or_default();
        if base_command == "sed" && !sed_command_is_allowed(command, self.policy.accept_edits) {
            return false;
        }
        if self.policy.accept_edits && ACCEPT_EDITS_ALLOWED_COMMANDS.contains(&base_command) {
            return true;
        }
        is_read_only_subcommand(command, compound_has_cd, self.context.cwd)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fixture {
        _directory: tempfile::TempDir,
        root: PathBuf,
    }

    impl Fixture {
        fn new() -> Self {
            let directory = tempfile::tempdir().unwrap();
            let root = directory.path().canonicalize().unwrap();
            std::fs::create_dir(root.join("src")).unwrap();
            std::fs::write(root.join("notes.txt"), "a\n").unwrap();
            Self {
                root,
                _directory: directory,
            }
        }

        fn allowed(&self, command: &str, accept_edits: bool) -> bool {
            let context = ShellPermissionContext {
                cwd: &self.root,
                working_directories: vec![self.root.clone()],
                readable_directories: &[],
            };
            is_allowed_without_rules(command, &context, accept_edits)
        }
    }

    #[test]
    fn read_only_compound_commands_are_allowed_in_every_mode() {
        let fixture = Fixture::new();
        for command in [
            "git diff --cached --stat | head -30",
            "cat notes.txt | grep foo",
            "ls -la && git status",
            "grep -rn \"x\" src 2>/dev/null",
            "git log --oneline | head -5; git status",
            "pwd\nls",
            "ls > /dev/null 2>&1",
            "cd src && ls",
            "rg -n foo src | sort | uniq -c",
        ] {
            assert!(fixture.allowed(command, false), "default: {command}");
            assert!(fixture.allowed(command, true), "accept edits: {command}");
        }
        let cd_to_cwd = format!("cd {} && git status", fixture.root.display());
        assert!(fixture.allowed(&cd_to_cwd, false));
    }

    #[test]
    fn unsafe_compound_commands_are_not_allowed() {
        let fixture = Fixture::new();
        for command in [
            "cd src && git status",
            "cd src && cd ..",
            "cd src | git log",
            "ls && python3 script.py",
            "cat notes.txt | sh",
            "ls > out.txt",
            "ls > out.txt | cat",
            "echo $(id)",
            "cat /etc/passwd | head",
            "ls \\; pwd",
            "grep foo notes.txt\n# ; rm -rf src",
        ] {
            assert!(!fixture.allowed(command, false), "default: {command}");
        }
    }

    #[test]
    fn accept_edits_allows_workspace_filesystem_commands_with_claude_code_constraints() {
        let fixture = Fixture::new();
        for command in [
            "mkdir -p build/out",
            "touch notes.txt",
            "rm -rf build",
            "rmdir build",
            "mv notes.txt renamed.txt",
            "cp notes.txt copy.txt",
            "sed -i 's/a/b/g' notes.txt",
            "mkdir build && touch build/a.txt",
            "mkdir build && ls build",
            "ls > out.txt",
            "echo done > log.txt",
        ] {
            assert!(fixture.allowed(command, true), "accept edits: {command}");
            assert!(!fixture.allowed(command, false), "default: {command}");
        }
        for command in [
            "touch ../outside.txt",
            "mv -f notes.txt renamed.txt",
            "cp -r src dst",
            "cd src && touch a.txt",
            "cd src && echo x > a.txt",
            "rm -rf *",
            "rm -rf ~",
            "rm -rf /",
            "rm -rf /usr",
            "touch .git/config",
            "sed -i 's/a/b/w out' notes.txt",
            "sed -i -e 'e id' notes.txt",
            "mkdir build && python3 build.py",
            "mkdir $DIR",
            "timeout 5 rm -rf build",
            "chmod +x notes.txt",
            "ln -s notes.txt link",
        ] {
            assert!(!fixture.allowed(command, true), "accept edits: {command}");
        }
    }

    #[test]
    fn read_only_sed_follows_claude_code_mode_split() {
        let fixture = Fixture::new();
        assert!(fixture.allowed("sed -n '1p' notes.txt", false));
        assert!(!fixture.allowed("sed -n '1p' notes.txt", true));
        assert!(!fixture.allowed("sed 's/a/b/' notes.txt", false));
        assert!(fixture.allowed("sed 's/a/b/' notes.txt", true));
    }
}
