//! Opens of hosted Ticks for tests that run a private Tickhost: each Tick starts through the
//! fixture CLI's Tick launcher, as the daemon starts one.

use std::path::{Path, PathBuf};

use tickhost::host::{OpenRequest, TickOwner};
use uuid::Uuid;

/// The environment a hosted test Tick runs with.
pub fn tick_environment() -> Vec<(String, String)> {
    vec![("PATH".into(), "/usr/bin:/bin".into())]
}

/// `program` started through the fixture CLI's Tick launcher.
pub fn launch_command(program: Vec<String>) -> Vec<String> {
    let mut command = vec![
        super::fixture_cli::executable()
            .to_string_lossy()
            .into_owned(),
        "--tick-launch".into(),
        "--".into(),
    ];
    command.extend(program);
    command
}

/// Python running `script` unbuffered with `arguments`.
pub fn python_program(script: &Path, arguments: &[String]) -> Vec<String> {
    let mut program = vec![
        "/usr/bin/python3".into(),
        "-u".into(),
        script.to_string_lossy().into_owned(),
    ];
    program.extend_from_slice(arguments);
    program
}

/// The example counter Tick.
pub fn counter_program() -> Vec<String> {
    let counter = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../examples/ticks/counter.py")
        .canonicalize()
        .unwrap();
    python_program(&counter, &[])
}

/// An open of `program` for `owner`, started through the fixture CLI's Tick launcher.
pub fn tick_request(owner: TickOwner, cwd: &Path, program: Vec<String>) -> OpenRequest {
    OpenRequest {
        operation: Uuid::new_v4(),
        owner,
        owner_revision: 0,
        command: launch_command(program),
        cwd: cwd.to_path_buf(),
        environment: tick_environment(),
        title: None,
    }
}

/// An open of the example counter Tick for `owner`.
pub fn counter_request(owner: TickOwner, cwd: &Path) -> OpenRequest {
    tick_request(owner, cwd, counter_program())
}
