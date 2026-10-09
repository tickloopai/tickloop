//! Internal transport fixture executable; the shipped tickloop binary never reads these files.
use std::path::PathBuf;

/// The fixture's first argument that runs the product CLI; any other unknown first argument is
/// taken as a libtest probe.
pub const CLI_MODE: &str = "--cli";

/// The fixture executable, for callers that pass a product process mode themselves.
pub fn executable() -> PathBuf {
    let directory = std::env::current_exe()
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf();
    std::fs::read_dir(directory)
        .unwrap()
        .filter_map(Result::ok)
        .filter(|entry| {
            entry
                .file_name()
                .to_string_lossy()
                .starts_with("cli_fixture-")
                && entry.path().extension().is_none()
                && entry.path().is_file()
        })
        .max_by_key(|entry| entry.metadata().unwrap().modified().unwrap())
        .expect("build the transport fixture with cargo test --test cli_fixture --no-run")
        .path()
}

/// The product CLI through the fixture; callers append CLI arguments.
pub fn command() -> std::process::Command {
    let mut command = std::process::Command::new(executable());
    command.arg(CLI_MODE);
    command
}

pub fn tokio_command() -> tokio::process::Command {
    let mut command = tokio::process::Command::new(executable());
    command.arg(CLI_MODE);
    command
}

pub fn pty_command() -> portable_pty::CommandBuilder {
    let mut command = portable_pty::CommandBuilder::new(executable());
    command.arg(CLI_MODE);
    command
}
