//! Every tickhost integration test, compiled as one binary.

#[path = "all/cli.rs"]
mod cli;
#[path = "all/hosted_launch.rs"]
mod hosted_launch;
#[path = "all/protocol.rs"]
mod protocol;
#[path = "all/shell_tasks.rs"]
mod shell_tasks;
#[path = "all/spawn_paths.rs"]
mod spawn_paths;
