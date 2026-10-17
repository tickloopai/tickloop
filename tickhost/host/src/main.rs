use std::process::ExitCode;

fn main() -> ExitCode {
    if std::env::args_os().skip(1).collect::<Vec<_>>() == ["--build-identity"] {
        println!("{}", tickhost::host::BUILD_IDENTITY);
        return ExitCode::SUCCESS;
    }
    tickhost::cli::run(std::env::args_os().skip(1))
}
