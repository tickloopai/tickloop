use std::os::unix::fs::PermissionsExt;

/// A short private directory owned by this fixture, never a sweep of another test's files.
pub fn runtime_dir() -> tempfile::TempDir {
    let directory = tempfile::Builder::new()
        .prefix("tlt-")
        .permissions(std::fs::Permissions::from_mode(0o700))
        .tempdir_in(tickloop_harness::test_support::SHORT_TEMP_ROOT)
        .unwrap();
    tickloop_harness::test_support::register_runtime_directory(directory.path());
    directory
}

pub(super) fn is_dead(process_id: libc::pid_t) -> bool {
    let signalled = unsafe { libc::kill(process_id, 0) };
    signalled == -1 && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
}
