//! The names people see for Tick Instances: a Tick by its declared title and the arguments that
//! tell its instances apart, a launch command by its program's file name, and a Herdr Pane by
//! the program it runs.

use std::path::Path;

use serde_json::Value;

use crate::herdr::Pane;

const NAME_SEPARATOR: &str = " · ";
/// `tickhost open -- PROGRAM ARGS…` launches PROGRAM; the name is PROGRAM's.
const LAUNCHER_PROGRAM: &str = "tickhost";
const LAUNCHER_OPEN: &str = "open";
const ARGUMENTS_SEPARATOR: &str = "--";
const HOME_PREFIX: &str = "~";
const ELIDED_DIRECTORIES: &str = "…";
/// The trailing directories a working directory keeps once it is shortened.
const KEPT_DIRECTORIES: usize = 2;

/// A launch command as people read it: the program it runs and its arguments, every absolute
/// path by its file name. A program started through `tickhost open --` is named itself.
pub fn command_title(words: &[String]) -> String {
    let launched = match words {
        [launcher, open, rest @ ..]
            if file_name(launcher) == LAUNCHER_PROGRAM && open == LAUNCHER_OPEN =>
        {
            rest.iter()
                .position(|word| word == ARGUMENTS_SEPARATOR)
                .map_or(rest, |separator| &rest[separator + 1..])
        }
        _ => words,
    };
    launched
        .iter()
        .map(|word| {
            if word.starts_with('/') {
                file_name(word)
            } else {
                word
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// A Tick's name: its declared title and the positional arguments of `command` (a
/// [`command_title`]) that follow the word naming its type, such as `Quote · AAPL.US` for
/// `tick longbridge quote AAPL.US`. A Tick without such arguments is named by its title alone.
pub fn tick_title(title: &str, tick_type: &str, command: &str) -> String {
    let type_names = [tick_type, tick_type.rsplit('.').next().unwrap_or(tick_type)];
    let names_type = |word: &&str| {
        let stem = Path::new(word)
            .file_stem()
            .and_then(|stem| stem.to_str())
            .unwrap_or(word);
        type_names
            .iter()
            .any(|name| name.eq_ignore_ascii_case(word) || name.eq_ignore_ascii_case(stem))
    };
    let arguments = command.split_whitespace().skip(1).collect::<Vec<_>>();
    let start = arguments
        .iter()
        .rposition(names_type)
        .map_or(0, |index| index + 1);
    let distinguishing = arguments[start..]
        .iter()
        .take_while(|word| !word.starts_with('-'))
        .copied()
        .collect::<Vec<_>>();
    if distinguishing.is_empty() {
        title.to_owned()
    } else {
        format!("{title}{NAME_SEPARATOR}{}", distinguishing.join(" "))
    }
}

/// One process of a Pane's foreground process group, as Herdr reports it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ForegroundProcess {
    pub pid: u64,
    pub argv: Vec<String>,
    pub name: Option<String>,
}

/// The foreground of a Pane from Herdr's `pane.process_info` reply.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PaneForeground {
    pub group: Option<u64>,
    pub shell_pid: Option<u64>,
    pub processes: Vec<ForegroundProcess>,
}

impl PaneForeground {
    pub fn from_process_info(process_info: &Value) -> Self {
        let processes = process_info["foreground_processes"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|process| {
                Some(ForegroundProcess {
                    pid: process["pid"].as_u64()?,
                    argv: process["argv"]
                        .as_array()?
                        .iter()
                        .map(|word| word.as_str().map(str::to_owned))
                        .collect::<Option<Vec<_>>>()?,
                    name: process["name"].as_str().map(str::to_owned),
                })
            })
            .collect();
        Self {
            group: process_info["foreground_process_group_id"].as_u64(),
            shell_pid: process_info["shell_pid"].as_u64(),
            processes,
        }
    }

    /// The process that leads the foreground group, or the first one when none leads it.
    fn leader(&self) -> Option<&ForegroundProcess> {
        self.processes
            .iter()
            .find(|process| Some(process.pid) == self.group)
            .or_else(|| self.processes.first())
    }

    /// The command the foreground runs, by its program's file name.
    pub fn command_title(&self) -> Option<String> {
        self.leader()
            .map(|process| command_title(&process.argv))
            .filter(|command| !command.is_empty())
    }
}

/// A Herdr Pane by what it runs: a program's own terminal title when it set one, else the
/// program's command; an idle shell by its name and shortened working directory, such as
/// `zsh · ~/…/tickloop/feat-uipolish`.
pub fn pane_title(pane: &Pane, foreground: &PaneForeground) -> Option<String> {
    let leader = foreground.leader()?;
    if foreground.shell_pid == Some(leader.pid) {
        let shell = leader
            .name
            .as_deref()
            .unwrap_or_else(|| leader.argv.first().map_or("", String::as_str));
        let shell = file_name(shell.trim_start_matches('-'));
        return Some(
            match pane.foreground_cwd.as_deref().or(pane.cwd.as_deref()) {
                Some(directory) => {
                    format!("{shell}{NAME_SEPARATOR}{}", short_directory(directory))
                }
                None => shell.to_owned(),
            },
        );
    }
    let command = command_title(&leader.argv);
    pane.terminal_title_stripped
        .as_deref()
        .map(str::trim)
        .filter(|title| is_program_title(title, &command))
        .map(str::to_owned)
        .or_else(|| (!command.is_empty()).then_some(command))
}

/// Whether a terminal title names what the program shows rather than where a shell is: not a
/// path, not a `user@host:path` prompt and not just the program's own name.
fn is_program_title(title: &str, command: &str) -> bool {
    let program = command.split_whitespace().next().unwrap_or_default();
    !title.is_empty()
        && !title.starts_with('/')
        && !title.starts_with(HOME_PREFIX)
        && !title
            .split_once(':')
            .is_some_and(|(user_host, _)| user_host.contains('@'))
        && title != program
}

/// `directory` with the home directory as `~` and only its last [`KEPT_DIRECTORIES`] names.
fn short_directory(directory: &str) -> String {
    let home = std::env::var("HOME").ok().filter(|home| !home.is_empty());
    let (root, rest) = match home
        .as_deref()
        .and_then(|home| directory.strip_prefix(home))
        .filter(|rest| rest.is_empty() || rest.starts_with('/'))
    {
        Some(rest) => (HOME_PREFIX, rest),
        None => ("", directory),
    };
    let names = rest
        .split('/')
        .filter(|name| !name.is_empty())
        .collect::<Vec<_>>();
    if names.len() <= KEPT_DIRECTORIES {
        let joined = names.join("/");
        return match (root, joined.is_empty()) {
            ("", true) => "/".into(),
            (root, true) => root.into(),
            (root, false) => format!("{root}/{joined}"),
        };
    }
    format!(
        "{root}/{ELIDED_DIRECTORIES}/{}",
        names[names.len() - KEPT_DIRECTORIES..].join("/")
    )
}

fn file_name(program: &str) -> &str {
    Path::new(program)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(program)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn words(command: &str) -> Vec<String> {
        command.split_whitespace().map(str::to_owned).collect()
    }

    fn pane(title: Option<&str>, cwd: &str) -> Pane {
        Pane {
            pane_id: "w1:p1".into(),
            terminal_id: "term".into(),
            workspace_id: "w1".into(),
            tab_id: "w1:t1".into(),
            cwd: Some(cwd.into()),
            foreground_cwd: Some(cwd.into()),
            terminal_title_stripped: title.map(str::to_owned),
            tokens: Default::default(),
        }
    }

    fn foreground(shell_pid: u64, group: u64, processes: Value) -> PaneForeground {
        PaneForeground::from_process_info(&json!({
            "foreground_process_group_id": group,
            "foreground_processes": processes,
            "shell_pid": shell_pid,
        }))
    }

    #[test]
    fn a_launch_command_names_its_program_and_paths_by_file_name() {
        for (command, expected) in [
            (
                "/Users/me/tickloop/target/debug/tick longbridge quote AAPL.US",
                "tick longbridge quote AAPL.US",
            ),
            (
                "/opt/bin/tickhost open -- python3 /Users/me/tick/examples/counter.py --start 3",
                "python3 counter.py --start 3",
            ),
            ("npm run dev", "npm run dev"),
        ] {
            assert_eq!(command_title(&words(command)), expected, "{command}");
        }
        assert_eq!(command_title(&[]), "");
    }

    #[test]
    fn a_tick_is_named_by_its_title_and_the_arguments_after_its_type() {
        for (title, tick_type, command, expected) in [
            (
                "Quote",
                "quote",
                "tick longbridge quote AAPL.US",
                "Quote · AAPL.US",
            ),
            (
                "Quote",
                "quote",
                "tick longbridge quote AAPL.US 700.HK --interval 5",
                "Quote · AAPL.US 700.HK",
            ),
            (
                "Positions",
                "positions",
                "tick longbridge positions",
                "Positions",
            ),
            (
                "Counter",
                "example.counter",
                "python3 counter.py --start 3",
                "Counter",
            ),
            (
                "Weather",
                "external.weather",
                "weather Paris",
                "Weather · Paris",
            ),
        ] {
            assert_eq!(tick_title(title, tick_type, command), expected, "{command}");
        }
    }

    #[test]
    fn a_running_program_is_named_by_its_command_unless_it_titled_its_terminal() {
        let make = foreground(
            10,
            20,
            json!([
                {"pid": 21, "argv": ["/Users/me/bin/tickloop"], "name": "tickloop"},
                {"pid": 20, "argv": ["/usr/bin/make", "dev"], "name": "gnumake"},
            ]),
        );
        assert_eq!(
            pane_title(
                &pane(Some("me@host:~/tickloop"), "/Users/me/tickloop"),
                &make
            )
            .as_deref(),
            Some("make dev")
        );
        assert_eq!(
            pane_title(
                &pane(Some("/Users/me/tickloop"), "/Users/me/tickloop"),
                &make
            )
            .as_deref(),
            Some("make dev")
        );
        let claude = foreground(
            10,
            30,
            json!([{"pid": 30, "argv": ["claude", "--resume", "abc"], "name": "2.1.294"}]),
        );
        assert_eq!(
            pane_title(&pane(Some("WAF bypass 安全问题"), "/Users/me"), &claude).as_deref(),
            Some("WAF bypass 安全问题")
        );
        assert_eq!(
            pane_title(&pane(Some("claude"), "/Users/me"), &claude).as_deref(),
            Some("claude --resume abc")
        );
    }

    #[test]
    fn an_idle_shell_is_named_by_its_shell_and_shortened_directory() {
        let home = std::env::var("HOME").unwrap();
        let idle = foreground(
            986,
            986,
            json!([{"pid": 986, "argv": ["-zsh"], "name": "zsh"}]),
        );
        for (directory, expected) in [
            (
                format!("{home}/.herdr/worktrees/tickloop/feat-uipolish"),
                "zsh · ~/…/tickloop/feat-uipolish",
            ),
            (format!("{home}/dev/notes"), "zsh · ~/dev/notes"),
            (home.clone(), "zsh · ~"),
            ("/opt/homebrew/bin".to_owned(), "zsh · /…/homebrew/bin"),
            ("/".to_owned(), "zsh · /"),
        ] {
            assert_eq!(
                pane_title(&pane(Some("me@host:~"), &directory), &idle).as_deref(),
                Some(expected),
                "{directory}"
            );
        }
    }
}
