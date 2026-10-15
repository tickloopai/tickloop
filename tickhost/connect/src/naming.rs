//! The names people see for Tick Instances: a Tick by its declared title and the arguments that
//! tell its instances apart, a launch command by its program's file name, and a working directory
//! shortened to its last names.

use std::path::Path;

const NAME_SEPARATOR: &str = " · ";
const HOME_PREFIX: &str = "~";
const ELIDED_DIRECTORIES: &str = "…";
/// The trailing directories a working directory keeps once it is shortened.
const KEPT_DIRECTORIES: usize = 2;

/// A launch command as people read it: the program it runs and its arguments, every absolute
/// path by its file name.
pub fn command_title(words: &[String]) -> String {
    words
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
/// `tick-longbridge quote AAPL.US`. A Tick without such arguments is named by its title alone.
pub fn tick_title(title: &str, kind: &str, command: &str) -> String {
    let type_names = [kind, kind.rsplit('.').next().unwrap_or(kind)];
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

/// `directory` with the home directory as `~` and only its last [`KEPT_DIRECTORIES`] names,
/// such as `~/…/tickloop/feat-uipolish`.
pub fn short_directory(directory: &str) -> String {
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

    fn words(command: &str) -> Vec<String> {
        command.split_whitespace().map(str::to_owned).collect()
    }

    #[test]
    fn a_launch_command_names_its_program_and_paths_by_file_name() {
        for (command, expected) in [
            (
                "/Users/me/tickloop/target/debug/tick-longbridge quote AAPL.US",
                "tick-longbridge quote AAPL.US",
            ),
            (
                "/usr/bin/python3 /Users/me/tick/examples/counter.py --start 3",
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
        for (title, kind, command, expected) in [
            (
                "Quote",
                "quote",
                "tick-longbridge quote AAPL.US",
                "Quote · AAPL.US",
            ),
            (
                "Quote",
                "quote",
                "tick-longbridge quote AAPL.US 700.HK --interval 5",
                "Quote · AAPL.US 700.HK",
            ),
            (
                "Positions",
                "positions",
                "tick-longbridge positions",
                "Positions",
            ),
            ("Reader", "reader", "tick-reader ./a.md", "Reader · ./a.md"),
            (
                "Reader",
                "reader",
                "tick-reader ./a.md https://example.com/b.html --symbol AAPL.US",
                "Reader · ./a.md https://example.com/b.html",
            ),
            (
                "Futu Options",
                "options",
                "tick-futu options AAPL.US --port 11111",
                "Futu Options · AAPL.US",
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
            assert_eq!(tick_title(title, kind, command), expected, "{command}");
        }
    }

    #[test]
    fn a_directory_is_shortened_to_its_last_names_under_home() {
        let home = std::env::var("HOME").unwrap();
        for (directory, expected) in [
            (
                format!("{home}/.herdr/worktrees/tickloop/feat-uipolish"),
                "~/…/tickloop/feat-uipolish",
            ),
            (format!("{home}/dev/notes"), "~/dev/notes"),
            (home.clone(), "~"),
            ("/opt/homebrew/bin".to_owned(), "/…/homebrew/bin"),
            ("/".to_owned(), "/"),
        ] {
            assert_eq!(short_directory(&directory), expected, "{directory}");
        }
    }
}
