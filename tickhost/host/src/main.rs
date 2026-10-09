use std::process::ExitCode;

fn main() -> ExitCode {
    tickhost::cli::run(std::env::args_os().skip(1))
}
