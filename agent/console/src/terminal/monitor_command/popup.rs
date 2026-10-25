//! `tickloop monitor popup install|remove`: the explicit, consented commands that install Tick
//! Loop's Herdr popup plugin into one Herdr session or remove it again.

use std::{
    io::{self, BufRead, IsTerminal, Write},
    path::Path,
};

use crate::terminal::popup::{HerdrSession, PLUGIN_ID, PopupPlugin};
use tickloop_agent_settings::{UserSettings, config_root_directory};

const INSTALL_PROMPT: &str = "This registers Tickloop's popup plugin in the given Herdr session; no Herdr or Tick service is started, stopped or replaced. Install? [y/N]";
const REMOVE_PROMPT: &str = "This removes Tickloop's popup plugin tickloop.monitor from the given Herdr session and sets monitorPopup to disabled; enter in Monitor then opens Ticks full screen in the current terminal, and running Ticks keep running. Remove? [y/N]";
const NOTHING_CHANGED: &str = "nothing was changed";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum PopupChange {
    Install,
    Remove,
}

pub(super) async fn run(change: PopupChange, herdr_socket: &Path) -> Result<(), String> {
    let root = config_root_directory()
        .ok_or("cannot find the Tickloop state directory; set TICKLOOP_CONFIG_DIR")?;
    let plugin = PopupPlugin::for_this_executable(&root)?;
    let herdr = HerdrSession::verify(herdr_socket).await?;
    let settings = UserSettings::in_root(&root);
    match change {
        PopupChange::Install => {
            herdr.refuse_foreign(&plugin).await.map_err(unchanged)?;
            confirm(INSTALL_PROMPT)?;
            herdr.install(&plugin, &settings).await?;
            println!(
                "Registered Tickloop's popup plugin {PLUGIN_ID} in {}; monitorPopup is enabled.",
                herdr.socket().display()
            );
        }
        PopupChange::Remove => {
            herdr.require_ours(&plugin).await.map_err(unchanged)?;
            confirm(REMOVE_PROMPT)?;
            herdr.remove(&plugin, &settings).await?;
            println!(
                "Removed Tickloop's popup plugin {PLUGIN_ID} from {}; monitorPopup is disabled and running Ticks keep running.",
                herdr.socket().display()
            );
        }
    }
    Ok(())
}

fn unchanged(reason: String) -> String {
    format!("{reason}; {NOTHING_CHANGED}")
}

/// Asks `prompt` on this terminal; only an explicit yes goes ahead.
fn confirm(prompt: &str) -> Result<(), String> {
    let stdin = io::stdin();
    if !stdin.is_terminal() {
        return Err(format!(
            "confirming needs an interactive terminal; {NOTHING_CHANGED}"
        ));
    }
    print!("{prompt} ");
    io::stdout().flush().map_err(|error| error.to_string())?;
    match read_consent(&mut stdin.lock()) {
        Ok(true) => Ok(()),
        Ok(false) => Err(format!("cancelled; {NOTHING_CHANGED}")),
        Err(error) => Err(format!(
            "cannot read the answer ({error}); {NOTHING_CHANGED}"
        )),
    }
}

/// Reads one answer line: only `y` or `yes`, in any case, consents. An empty line or EOF is No.
fn read_consent(input: &mut impl BufRead) -> io::Result<bool> {
    let mut answer = String::new();
    input.read_line(&mut answer)?;
    Ok(matches!(
        answer.trim().to_ascii_lowercase().as_str(),
        "y" | "yes"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_an_explicit_yes_consents() {
        for (answer, consents) in [
            ("y\n", true),
            ("yes\n", true),
            ("Y\n", true),
            ("YES\r\n", true),
            (" y \n", true),
            ("y", true),
            ("\n", false),
            ("", false),
            ("n\n", false),
            ("no\n", false),
            ("yess\n", false),
            ("ok\n", false),
            ("y y\n", false),
        ] {
            assert_eq!(
                read_consent(&mut io::Cursor::new(answer)).unwrap(),
                consents,
                "{answer:?}"
            );
        }
    }
}
