use std::process::ExitCode;

fn main() -> ExitCode {
    tickloop_ticks::entry::run(std::env::args_os().skip(1))
}
