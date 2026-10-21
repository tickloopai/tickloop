//! Every tickhost integration test, compiled as one binary.

#[path = "all/cli.rs"]
mod cli;
#[path = "all/content_identity.rs"]
mod content_identity;
#[path = "all/hosted_launch.rs"]
mod hosted_launch;
#[path = "all/protocol.rs"]
mod protocol;
#[path = "all/shell_tasks.rs"]
mod shell_tasks;
#[path = "all/spawn_paths.rs"]
mod spawn_paths;

/// A short canonical directory for test sockets, whose paths are limited to about 100 bytes;
/// macOS's `/tmp` is a symbolic link to `/private/tmp`.
#[cfg(target_os = "macos")]
const SHORT_TEMP_ROOT: &str = "/private/tmp";
#[cfg(not(target_os = "macos"))]
const SHORT_TEMP_ROOT: &str = "/tmp";
