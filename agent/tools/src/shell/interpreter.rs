//! The shell that runs commands, chosen the way Claude Code's `findSuitableShell`
//! (`utils/Shell.ts`) does. Commands run as `<shell> -c <command>`.

use std::{
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    sync::OnceLock,
};

/// Names the shell to use, like Claude Code's `CLAUDE_CODE_SHELL`.
const SHELL_OVERRIDE_VARIABLE: &str = "TICKLOOP_SHELL";
const LOGIN_SHELL_VARIABLE: &str = "SHELL";
const SEARCH_PATH_VARIABLE: &str = "PATH";
const BASH: &str = "bash";
const ZSH: &str = "zsh";
/// Where Claude Code looks for each shell after the `which` results.
const SHELL_DIRECTORIES: [&str; 4] = ["/bin", "/usr/bin", "/usr/local/bin", "/opt/homebrew/bin"];
const EXECUTABLE_MODE_BITS: u32 = 0o111;
pub(super) const NO_SUITABLE_SHELL: &str = "No suitable shell found. Tickloop runs commands with bash or zsh. Please ensure one of them is installed and the SHELL environment variable is set, or set TICKLOOP_SHELL to its path.";

/// The shell this process runs commands with, resolved once from its environment.
pub(super) fn selected_shell() -> Result<PathBuf, String> {
    static SELECTED: OnceLock<Result<PathBuf, String>> = OnceLock::new();
    SELECTED
        .get_or_init(|| {
            let override_shell = std::env::var(SHELL_OVERRIDE_VARIABLE).ok();
            let login_shell = std::env::var(LOGIN_SHELL_VARIABLE).ok();
            find_suitable_shell(
                override_shell.as_deref(),
                login_shell.as_deref(),
                which,
                is_executable,
            )
        })
        .clone()
}

/// Claude Code's order: an executable bash or zsh override, then an executable bash or zsh
/// `$SHELL`, then the `which` result for the shell `$SHELL` prefers (zsh unless it names bash),
/// both shells in the fixed directories in that preference, and finally the other `which` result.
pub(super) fn find_suitable_shell(
    override_shell: Option<&str>,
    login_shell: Option<&str>,
    which: impl Fn(&str) -> Option<PathBuf>,
    is_executable: impl Fn(&Path) -> bool,
) -> Result<PathBuf, String> {
    if let Some(shell) = override_shell.filter(|shell| is_supported(shell))
        && is_executable(Path::new(shell))
    {
        return Ok(shell.into());
    }
    let login_shell = login_shell.filter(|shell| is_supported(shell));
    let (preferred, other) = if login_shell.is_some_and(|shell| shell.contains(BASH)) {
        (BASH, ZSH)
    } else {
        (ZSH, BASH)
    };
    login_shell
        .map(PathBuf::from)
        .into_iter()
        .chain(which(preferred))
        .chain([preferred, other].into_iter().flat_map(|name| {
            SHELL_DIRECTORIES
                .iter()
                .map(move |directory| Path::new(directory).join(name))
        }))
        .chain(which(other))
        .find(|candidate| is_executable(candidate))
        .ok_or_else(|| NO_SUITABLE_SHELL.to_owned())
}

fn is_supported(shell: &str) -> bool {
    shell.contains(BASH) || shell.contains(ZSH)
}

/// The first executable `name` in an absolute `PATH` entry. Relative entries are skipped: commands
/// change into their own working directory before the shell starts.
fn which(name: &str) -> Option<PathBuf> {
    let search_path = std::env::var_os(SEARCH_PATH_VARIABLE)?;
    std::env::split_paths(&search_path)
        .filter(|directory| directory.is_absolute())
        .map(|directory| directory.join(name))
        .find(|candidate| is_executable(candidate))
}

fn is_executable(path: &Path) -> bool {
    std::fs::metadata(path).is_ok_and(|metadata| {
        metadata.is_file() && metadata.permissions().mode() & EXECUTABLE_MODE_BITS != 0
    })
}

#[cfg(test)]
mod tests {
    use std::collections::{HashMap, HashSet};

    use super::*;

    struct System {
        which: HashMap<&'static str, &'static str>,
        executables: HashSet<&'static str>,
    }

    impl System {
        fn new(which: &[(&'static str, &'static str)], executables: &[&'static str]) -> Self {
            Self {
                which: which.iter().copied().collect(),
                executables: executables.iter().copied().collect(),
            }
        }

        fn find(&self, override_shell: Option<&str>, login_shell: Option<&str>) -> String {
            find_suitable_shell(
                override_shell,
                login_shell,
                |name| self.which.get(name).map(PathBuf::from),
                |path| self.executables.contains(path.to_str().unwrap()),
            )
            .map(|path| path.display().to_string())
            .unwrap_or_else(|error| error)
        }
    }

    #[test]
    fn an_executable_bash_or_zsh_override_wins() {
        let system = System::new(
            &[("zsh", "/bin/zsh")],
            &["/bin/zsh", "/custom/bash", "/custom/fish"],
        );
        assert_eq!(
            system.find(Some("/custom/bash"), Some("/bin/zsh")),
            "/custom/bash"
        );
        assert_eq!(
            system.find(Some("/custom/fish"), Some("/bin/zsh")),
            "/bin/zsh"
        );
        assert_eq!(
            system.find(Some("/missing/zsh"), Some("/bin/zsh")),
            "/bin/zsh"
        );
    }

    #[test]
    fn an_executable_bash_or_zsh_login_shell_comes_next() {
        let system = System::new(
            &[("zsh", "/bin/zsh"), ("bash", "/bin/bash")],
            &["/bin/zsh", "/bin/bash", "/opt/tools/bash", "/usr/bin/fish"],
        );
        assert_eq!(
            system.find(None, Some("/opt/tools/bash")),
            "/opt/tools/bash"
        );
        assert_eq!(system.find(None, Some("/usr/bin/fish")), "/bin/zsh");
        assert_eq!(system.find(None, Some("/missing/bash")), "/bin/bash");
        assert_eq!(system.find(Some(""), Some("")), "/bin/zsh");
    }

    #[test]
    fn the_search_prefers_the_login_shell_kind_then_fixed_directories() {
        let both = System::new(
            &[
                ("zsh", "/usr/local/bin/zsh"),
                ("bash", "/opt/homebrew/bin/bash"),
            ],
            &[
                "/usr/local/bin/zsh",
                "/opt/homebrew/bin/bash",
                "/bin/zsh",
                "/bin/bash",
            ],
        );
        assert_eq!(both.find(None, None), "/usr/local/bin/zsh");
        assert_eq!(
            both.find(None, Some("/missing/bash")),
            "/opt/homebrew/bin/bash"
        );

        let fixed_only = System::new(&[], &["/usr/bin/bash", "/opt/homebrew/bin/zsh"]);
        assert_eq!(fixed_only.find(None, None), "/opt/homebrew/bin/zsh");
        assert_eq!(
            fixed_only.find(None, Some("/missing/bash")),
            "/usr/bin/bash"
        );

        let other_which_last = System::new(&[("bash", "/nix/bin/bash")], &["/nix/bin/bash"]);
        assert_eq!(other_which_last.find(None, None), "/nix/bin/bash");
    }

    #[test]
    fn no_bash_or_zsh_explains_what_is_required() {
        let system = System::new(&[], &["/bin/sh", "/usr/bin/fish"]);
        assert_eq!(
            system.find(Some("/bin/sh"), Some("/usr/bin/fish")),
            NO_SUITABLE_SHELL
        );
    }
}
