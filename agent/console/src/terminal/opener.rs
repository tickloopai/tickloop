use std::process::{Command, Stdio};
#[cfg(test)]
use std::sync::{Arc, Mutex};
use std::thread;

#[cfg(target_os = "macos")]
const OPEN_COMMAND: &str = "open";
#[cfg(not(target_os = "macos"))]
const OPEN_COMMAND: &str = "xdg-open";

/// What a modified click on a link does with its target.
#[derive(Clone)]
pub(super) enum Opener {
    /// The terminal opens links on a modified click itself and still reports the click, as
    /// xterm.js does in VS Code and its forks; opening it here too would open it twice.
    Terminal,
    /// The desktop's own handler, run on the machine Tickloop runs on.
    System,
    #[cfg(test)]
    Captured(Arc<Mutex<Vec<String>>>),
}

impl Opener {
    pub(super) fn detect() -> Self {
        if std::env::var("TERM_PROGRAM").as_deref() == Ok("vscode") {
            Self::Terminal
        } else {
            Self::System
        }
    }

    pub(super) fn opens_links(&self) -> bool {
        !matches!(self, Self::Terminal)
    }

    /// Opens `url`. The handler is not waited for, so a slow browser never holds up input.
    pub(super) fn open(&self, url: &str) -> Result<(), String> {
        match self {
            Self::Terminal => Ok(()),
            Self::System => {
                let mut child = Command::new(OPEN_COMMAND)
                    .arg(url)
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .spawn()
                    .map_err(|error| format!("cannot run {OPEN_COMMAND}: {error}"))?;
                let _ = thread::Builder::new()
                    .name("agent-link-opener".into())
                    .spawn(move || child.wait());
                Ok(())
            }
            #[cfg(test)]
            Self::Captured(opened) => {
                opened
                    .lock()
                    .map_err(|error| error.to_string())?
                    .push(url.to_owned());
                Ok(())
            }
        }
    }
}
