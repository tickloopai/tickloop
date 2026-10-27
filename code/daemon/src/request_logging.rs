use std::{fs::OpenOptions, os::unix::fs::OpenOptionsExt, path::Path};
use tickloop_harness::base::private_fs::PrivateDirectory;

pub(crate) fn install(root: Option<&Path>) -> Result<(), String> {
    let Some(root) = root else {
        return Ok(());
    };
    let directory = PrivateDirectory::trusted(root.to_owned())
        .join("logs")
        .join("agents");
    directory
        .create()
        .map_err(|_| "cannot create Agent log directory")?;
    let name = format!(
        "worker-{}-{}.log",
        std::process::id(),
        tickloop_harness::base::private_fs::unix_time_milliseconds()
    );
    let file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(directory.path().join(name))
        .map_err(|_| "cannot create Agent timing log")?;
    let subscriber = tracing_subscriber::fmt()
        .with_ansi(false)
        .with_target(false)
        .with_writer(std::sync::Mutex::new(file))
        .with_max_level(tracing::Level::DEBUG)
        .finish();
    use tracing_subscriber::prelude::*;
    let subscriber = subscriber.with(
        tracing_subscriber::filter::Targets::new()
            .with_target("tickloop::request_timing", tracing::Level::DEBUG),
    );
    tracing::subscriber::set_global_default(subscriber)
        .map_err(|_| "cannot install Agent timing log".into())
}
