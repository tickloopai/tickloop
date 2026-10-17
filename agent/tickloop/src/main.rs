use std::io::{self, IsTerminal, Write};

use crossterm::style::Stylize;

const TERMINAL_WORKER_THREADS: usize = 2;

fn main() {
    let arguments = std::env::args_os().skip(1).collect::<Vec<_>>();
    if arguments.len() == 1 {
        match arguments[0].to_str() {
            Some("--version" | "-V") => {
                println!("tickloop {}", env!("CARGO_PKG_VERSION"));
                return;
            }
            Some("--build-identity") => {
                println!("{}", env!("TICKLOOP_BUILD_IDENTITY"));
                return;
            }
            _ => {}
        }
    }
    let build = tickloop_agent_updater::Build {
        version: env!("CARGO_PKG_VERSION").into(),
        identity: env!("TICKLOOP_BUILD_IDENTITY").into(),
    };
    let is_console = !arguments.first().is_some_and(|arg| {
        matches!(
            arg.to_str(),
            Some("update" | "daemon" | "worker" | "monitor" | "tick")
        )
    }) && !arguments
        .iter()
        .any(|arg| matches!(arg.to_str(), Some("--help" | "-h")));
    if is_console {
        match tickloop_agent_updater::prepare_launch(&build) {
            Ok(Some(executable)) => {
                use std::os::unix::process::CommandExt;
                let error = std::process::Command::new(executable)
                    .args(&arguments)
                    .exec();
                eprintln!("Error: Cannot open the pinned Tickloop runtime: {error}");
                std::process::exit(1);
            }
            Ok(None) => {}
            Err(error) => {
                eprintln!("Error: Cannot preserve this Tickloop runtime: {error:#}");
                std::process::exit(1);
            }
        }
    }
    tickloop_agent::name_this_build();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(TERMINAL_WORKER_THREADS)
        .enable_all()
        .build()
        .expect("cannot start the Agent terminal runtime");
    let result = runtime.block_on(async {
        if arguments.first().is_some_and(|arg| arg == "update") {
            return tickloop_agent_updater::command(&arguments[1..], build)
                .await
                .map_err(|error| format!("{error:#}"));
        }
        if is_console {
            // Automatic checks must never prevent opening a Console.
            let _ = tickloop_agent_updater::start_background(
                &arguments,
                tickloop_agent_updater::BackgroundCheck::Startup,
            );
            let background_arguments = arguments.clone();
            tokio::spawn(async move {
                loop {
                    tokio::time::sleep(tickloop_agent_updater::CHECK_INTERVAL).await;
                    let _ = tickloop_agent_updater::start_background(
                        &background_arguments,
                        tickloop_agent_updater::BackgroundCheck::Periodic,
                    );
                }
            });
        }
        match tickloop_agent::run_internal(&arguments).await {
            Some(result) => result,
            None => tickloop_agent_console::terminal::run().await,
        }
    });
    if let Err(error) = result {
        let stderr = io::stderr();
        let use_color = stderr.is_terminal()
            && std::env::var_os("NO_COLOR").is_none()
            && std::env::var("TERM").is_ok_and(|term| term != "dumb");
        let error = tickloop_agent_console::terminal::safe_text(&error);
        let (title, body) = error.split_once('\n').unwrap_or((&error, ""));
        let mut output = stderr.lock();
        if use_color {
            let _ = writeln!(output, "{}", format!("Error: {title}").red().bold());
        } else {
            let _ = writeln!(output, "Error: {title}");
        }
        if !body.is_empty() {
            let _ = writeln!(output, "{body}");
        }
        std::process::exit(1);
    }
}
