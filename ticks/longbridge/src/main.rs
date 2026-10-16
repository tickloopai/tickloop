use std::process::ExitCode;

fn main() -> ExitCode {
    tick_command::entry::run(tick_longbridge::entry::execute)
}
