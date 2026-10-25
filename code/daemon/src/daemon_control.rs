use std::ffi::OsString;
use tickloop_code_client::{daemon_endpoint::runtime_directory, daemon_stop::prepare};

const DAEMON_USAGE: &str = "Usage: tickloop daemon stop";
const DAEMON_HELP: &str = "Usage: tickloop daemon stop\n\n\
Stop the local Agent daemon and all of its workers, which ends every running Agent.\n\
Saved history is kept. Ticks in the Tickhost keep running.\n\
The next tickloop launch starts this build.\n\
  -h, --help  Show this help.\n\n\
Each config root (~/.tickloop, or TICKLOOP_CONFIG_DIR when it is set) has one daemon.";

pub async fn run(arguments: &[OsString]) -> Result<(), String> {
    let Some(command) = arguments.first().and_then(|value| value.to_str()) else {
        return Err(DAEMON_USAGE.into());
    };
    if matches!(command, "--help" | "-h") && arguments.len() == 1 {
        println!("{DAEMON_HELP}");
        return Ok(());
    }
    if command != "stop" {
        return Err(format!("Unknown daemon command. {DAEMON_USAGE}"));
    }
    let mut help = false;
    for argument in &arguments[1..] {
        match argument.to_str() {
            Some("--help" | "-h") => help = true,
            _ => return Err(format!("Invalid daemon stop option. {DAEMON_USAGE}")),
        }
    }
    if help {
        println!("{DAEMON_HELP}");
        return Ok(());
    }
    match prepare(&runtime_directory(), None).await? {
        Some(prepared) => {
            println!("Stopping the Agent daemon and its running Agents. Saved history is kept.");
            prepared.stop().await?;
            println!("Agent daemon stopped.");
        }
        None => println!("Agent daemon is not running."),
    }
    Ok(())
}
