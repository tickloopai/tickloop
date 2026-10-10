//! Opens of hosted Ticks for tests that run a private Tickhost: a protocol program starts behind
//! the fixture CLI's Tick launcher, which the daemon's host controller puts in front of it.

use std::{
    path::{Path, PathBuf},
    time::Duration,
};

use tickhost::host::{OpenRequest, TickAdapter, TickOwner};
use uuid::Uuid;

/// How long a test Tickhost lets an unused Tick idle: longer than any wait of a test step, and
/// short enough that a host a killed test left behind exits soon after its Ticks go idle.
pub const IDLE_TIMEOUT: Duration = Duration::from_secs(60);

/// The environment a hosted test Tick runs with.
pub fn tick_environment() -> Vec<(String, String)> {
    vec![("PATH".into(), "/usr/bin:/bin".into())]
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
        .join("../../tick/examples/counter.py")
        .canonicalize()
        .unwrap();
    python_program(&counter, &[])
}

/// An open of the protocol `program` for `owner`.
pub fn tick_request(owner: TickOwner, cwd: &Path, program: Vec<String>) -> OpenRequest {
    OpenRequest {
        operation: Uuid::new_v4(),
        owner,
        owner_revision: 0,
        adapter: TickAdapter::Protocol,
        command: program,
        cwd: cwd.to_path_buf(),
        environment: tick_environment(),
        title: None,
    }
}

/// An open of the example counter Tick for `owner`.
pub fn counter_request(owner: TickOwner, cwd: &Path) -> OpenRequest {
    tick_request(owner, cwd, counter_program())
}
