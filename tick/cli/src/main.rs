use std::process::ExitCode;

fn main() -> ExitCode {
    tick_cli::entry::run(std::env::args_os().skip(1))
}
