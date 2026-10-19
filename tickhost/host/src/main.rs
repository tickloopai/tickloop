use std::process::ExitCode;

fn main() -> ExitCode {
    let arguments = std::env::args_os().skip(1).collect::<Vec<_>>();
    if arguments.len() == 1 {
        match arguments[0].to_str() {
            Some("--build-identity") => {
                println!("{}", env!("TICKLOOP_BUILD_IDENTITY"));
                return ExitCode::SUCCESS;
            }
            Some("--content-identity") => {
                println!("{}", tickhost::host::BUILD_IDENTITY);
                return ExitCode::SUCCESS;
            }
            _ => {}
        }
    }
    let _lease = match tickhost_connect::executable_lease::retain_current_executable() {
        Ok(lease) => lease,
        Err(error) => {
            eprintln!("tickhost: cannot retain running executable: {error}");
            return ExitCode::FAILURE;
        }
    };
    tickhost::cli::run(std::env::args_os().skip(1))
}
