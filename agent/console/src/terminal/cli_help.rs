//! Root CLI help only. The adapters still own argument parsing; `/help` owns the Console guide.

use clap::{Arg, ArgAction, ColorChoice, Command};

pub(super) fn render() -> String {
    command().render_help().to_string()
}

fn flag(name: &'static str, help: &'static str) -> Arg {
    Arg::new(name)
        .long(name)
        .action(ArgAction::SetTrue)
        .help(help)
}

fn command() -> Command {
    Command::new("tickloop")
        .about("Tickloop - terminal AI agent")
        .override_usage(
            "tickloop [OPTIONS]\n       tickloop -p [OPTIONS] [PROMPT | -]\n       tickloop <COMMAND> [ARGS]",
        )
        .disable_help_flag(true)
        .disable_version_flag(true)
        .disable_help_subcommand(true)
        .color(ColorChoice::Never)
        .term_width(100)
        .subcommand(
            Command::new("acp")
                .about("Run the ACP stdio server")
                .display_order(0),
        )
        .subcommand(
            Command::new("update")
                .about("Check or install updates")
                .display_order(1),
        )
        .subcommand(
            Command::new("daemon")
                .about("Manage the local daemon; stop ends all its running Agents")
                .display_order(2),
        )
        .subcommand(
            Command::new("tick")
                .about("Manage Tick packages and logins")
                .display_order(3),
        )
        .subcommand(
            Command::new("monitor")
                .about("Manage the Herdr Monitor popup")
                .display_order(4),
        )
        .arg(flag("print", "Run one task and exit; see tickloop -p --help").short('p'))
        .arg(
            Arg::new("cwd")
                .long("cwd")
                .value_name("DIRECTORY")
                .help("Set the working directory (default: current directory)"),
        )
        .arg(flag("help", "Print help").short('h'))
        .arg(flag("version", "Print version").short('V'))
        .next_help_heading("Session options")
        .arg(
            flag("continue", "Resume the most recent Chat in this directory")
                .short('c'),
        )
        .arg(
            Arg::new("resume")
                .short('r')
                .long("resume")
                .value_name("CHAT")
                .num_args(0..=1)
                .help("Resume a Chat, or open the picker when CHAT is omitted"),
        )
        .arg(
            Arg::new("chat-id")
                .long("chat-id")
                .value_name("CHAT")
                .help("Open or create an exact saved Chat identity"),
        )
        .arg(flag("fork-session", "Fork a Chat resumed with -c or -r"))
        .arg(flag("no-session-persistence", "Do not save this Chat"))
        .next_help_heading("Permission options")
        .arg(
            Arg::new("mode")
                .long("mode")
                .value_name("MODE")
                .help("Execution mode: auto, ask, fullAccess or plan"),
        )
        .arg(flag("deny-on-prompt", "Deny actions that require human approval"))
        .arg(flag(
            "dangerously-skip-permissions",
            "Bypass permission checks; requires external isolation",
        ))
        .arg(flag(
            "working-directory-is-trusted",
            "Assert trust for this run only; does not save trust",
        ))
        .after_help(
            "Run tickloop to open the Console; use /provider to configure a provider.\n\
             In the Console, /help explains commands and keyboard shortcuts.\n\
             More: tickloop -p --help, tickloop acp --help, tickloop tick --help.",
        )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn root_help_layout_is_stable() {
        assert_eq!(
            render(),
            r#"Tickloop - terminal AI agent

Usage: tickloop [OPTIONS]
       tickloop -p [OPTIONS] [PROMPT | -]
       tickloop <COMMAND> [ARGS]

Commands:
  acp      Run the ACP stdio server
  update   Check or install updates
  daemon   Manage the local daemon; stop ends all its running Agents
  tick     Manage Tick packages and logins
  monitor  Manage the Herdr Monitor popup

Options:
  -p, --print            Run one task and exit; see tickloop -p --help
      --cwd <DIRECTORY>  Set the working directory (default: current directory)
  -h, --help             Print help
  -V, --version          Print version

Session options:
  -c, --continue                Resume the most recent Chat in this directory
  -r, --resume [<CHAT>]         Resume a Chat, or open the picker when CHAT is omitted
      --chat-id <CHAT>          Open or create an exact saved Chat identity
      --fork-session            Fork a Chat resumed with -c or -r
      --no-session-persistence  Do not save this Chat

Permission options:
      --mode <MODE>                   Execution mode: auto, ask, fullAccess or plan
      --deny-on-prompt                Deny actions that require human approval
      --dangerously-skip-permissions  Bypass permission checks; requires external isolation
      --working-directory-is-trusted  Assert trust for this run only; does not save trust

Run tickloop to open the Console; use /provider to configure a provider.
In the Console, /help explains commands and keyboard shortcuts.
More: tickloop -p --help, tickloop acp --help, tickloop tick --help.
"#,
        );
    }

    #[test]
    fn root_help_has_public_entries_but_not_the_interactive_manual() {
        command().debug_assert();
        let help = render();
        let sections = [
            "Usage:",
            "Commands:",
            "Options:",
            "Session options:",
            "Permission options:",
        ];
        let positions = sections.map(|section| help.find(section).unwrap());
        assert!(positions.windows(2).all(|pair| pair[0] < pair[1]), "{help}");
        for current in [
            "tickloop [OPTIONS]",
            "tickloop -p [OPTIONS] [PROMPT | -]",
            "tickloop <COMMAND> [ARGS]",
            "acp",
            "update",
            "daemon",
            "tick",
            "monitor",
            "-p, --print",
            "--cwd <DIRECTORY>",
            "-c, --continue",
            "-r, --resume [<CHAT>]",
            "--chat-id <CHAT>",
            "--fork-session",
            "--no-session-persistence",
            "--mode <MODE>",
            "auto, ask, fullAccess or plan",
            "--deny-on-prompt",
            "--dangerously-skip-permissions",
            "--working-directory-is-trusted",
            "-h, --help",
            "-V, --version",
            "all its running Agents",
            "does not save trust",
            "/provider",
            "/help",
            "tickloop -p --help",
            "tickloop acp --help",
            "tickloop tick --help",
        ] {
            assert!(help.contains(current), "missing {current}: {help}");
        }
        for hidden in [
            "daemon run",
            "worker",
            "monitor attach",
            "--view-handoff",
            "--build-identity",
            "--runtime-info",
            "--config",
            "--output-format",
            "--timeout",
            "--p ",
            "Subagents N",
            "ctrl+enter",
            "/loops new",
            "Commands and Skills:",
        ] {
            assert!(!help.contains(hidden), "unexpected {hidden}: {help}");
        }
        assert!(!help.contains('\x1b'));
        assert!(
            help.lines().count() <= 60,
            "root help should stay concise: {help}"
        );
        assert!(help.lines().all(|line| line.len() <= 100), "{help}");
    }
}
