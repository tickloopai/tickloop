use std::io::{self, IsTerminal, Write};

use crossterm::style::Stylize;

const TERMINAL_WORKER_THREADS: usize = 2;

fn main() {
    tickloop_agent::name_this_build();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(TERMINAL_WORKER_THREADS)
        .enable_all()
        .build()
        .expect("cannot start the Agent terminal runtime");
    let arguments = std::env::args_os().skip(1).collect::<Vec<_>>();
    let result = runtime.block_on(async {
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
