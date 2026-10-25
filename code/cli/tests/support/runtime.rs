use std::{
    os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt},
    path::{Path, PathBuf},
    sync::Once,
};

const TEMPORARY_ROOT: &str = "/tmp";
/// Short, like the unprefixed random name below it, so sockets of a daemon, its workers and a
/// Tickhost inside the directory stay below the Unix socket path limit.
const PROCESS_DIRECTORY_PREFIX: &str = "tlt-";

static REMOVE_ABANDONED: Once = Once::new();

/// A private directory, `/tmp/tlt-<pid>/<random>`, removed when dropped: a daemon runtime
/// directory or a config root.
///
/// A daemon steps down once its runtime directory is removed, so dropping this ends whatever
/// daemon a test left in it. The first call in each test process removes the directories of test
/// processes that died without dropping theirs, such as ones killed by a timeout, together with
/// the daemon and Tickhost runtime directories derived from them as config roots, which ends
/// their daemons too.
pub fn runtime_dir() -> tempfile::TempDir {
    REMOVE_ABANDONED.call_once(remove_directories_of_dead_processes);
    let process_directory = PathBuf::from(TEMPORARY_ROOT)
        .join(format!("{PROCESS_DIRECTORY_PREFIX}{}", std::process::id()));
    if let Err(error) = std::fs::DirBuilder::new()
        .mode(0o700)
        .create(&process_directory)
        && error.kind() != std::io::ErrorKind::AlreadyExists
    {
        panic!("cannot create {}: {error}", process_directory.display());
    }
    tempfile::Builder::new()
        .prefix("")
        .permissions(std::fs::Permissions::from_mode(0o700))
        .tempdir_in(&process_directory)
        .unwrap()
}

fn remove_directories_of_dead_processes() {
    let Ok(entries) = std::fs::read_dir(TEMPORARY_ROOT) else {
        return;
    };
    for entry in entries.flatten() {
        let process_id = entry
            .file_name()
            .to_str()
            .and_then(|name| name.strip_prefix(PROCESS_DIRECTORY_PREFIX))
            .and_then(|process_id| process_id.parse::<libc::pid_t>().ok())
            .filter(|process_id| *process_id > 0);
        if let Some(process_id) = process_id
            && is_dead(process_id)
            && is_own_directory(&entry.path())
        {
            remove_runtimes_of_config_roots(&entry.path());
            let _ = std::fs::remove_dir_all(entry.path());
        }
    }
}

fn remove_runtimes_of_config_roots(process_directory: &Path) {
    let Ok(entries) = std::fs::read_dir(process_directory) else {
        return;
    };
    for config_root in entries.flatten().map(|entry| entry.path()) {
        let daemon =
            tickloop_code_client::daemon_endpoint::runtime_directory_of(Some(&config_root));
        let host = tickhost::paths::HostPaths::of_config_root(Some(&config_root)).runtime;
        let _ = std::fs::remove_dir_all(daemon);
        let _ = std::fs::remove_dir_all(host);
    }
}

pub(super) fn is_dead(process_id: libc::pid_t) -> bool {
    let signalled = unsafe { libc::kill(process_id, 0) };
    signalled == -1 && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
}

fn is_own_directory(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok_and(|metadata| {
        metadata.file_type().is_dir() && metadata.uid() == unsafe { libc::geteuid() }
    })
}
