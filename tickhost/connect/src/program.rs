//! Where an Agent open finds the program of a Tick: a bundled executable beside the Agent first,
//! then PATH relative to the working directory.

use std::{
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
};

use crate::host_error::HostError;

/// The executables installed beside the Agent, which an open finds there before PATH.
pub const BUNDLED_EXECUTABLES: [&str; 2] = ["tick", "tickhost"];

/// Resolves a program the way an Agent open does: a bundled executable beside the Agent first,
/// then PATH relative to `cwd`.
pub fn resolve_executable(program: &str, cwd: &Path) -> Result<PathBuf, HostError> {
    resolve_executable_from(
        program,
        cwd,
        std::env::current_exe().ok().as_deref(),
        &std::env::var_os("PATH").unwrap_or_default(),
    )
}

pub fn bundled_tick_executable() -> Option<PathBuf> {
    resolve_executable("tick", &std::env::current_dir().ok()?).ok()
}

pub fn bundled_tickhost_executable() -> Option<PathBuf> {
    resolve_executable("tickhost", &std::env::current_dir().ok()?).ok()
}

fn resolve_executable_from(
    program: &str,
    cwd: &Path,
    agent_executable: Option<&Path>,
    search_path: &std::ffi::OsStr,
) -> Result<PathBuf, HostError> {
    let candidates = if program.contains('/') {
        vec![cwd.join(program)]
    } else {
        let companion = BUNDLED_EXECUTABLES
            .contains(&program)
            .then(|| agent_executable.and_then(Path::parent))
            .flatten()
            .map(|directory| directory.join(program));
        companion
            .into_iter()
            .chain(
                std::env::split_paths(search_path)
                    .map(|directory| cwd.join(directory).join(program)),
            )
            .collect()
    };
    candidates
        .into_iter()
        .find(|path| {
            path.metadata().is_ok_and(|metadata| {
                metadata.is_file() && metadata.permissions().mode() & 0o111 != 0
            })
        })
        .and_then(|path| path.canonicalize().ok())
        .ok_or_else(|| {
            HostError::before(
                "InvalidArguments",
                format!("executable not found: {program}"),
            )
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bundled_tick_resolution_uses_its_installed_companion_before_path() {
        let directory = tempfile::tempdir().unwrap();
        let installed = directory.path().join("installed");
        let path_bin = directory.path().join("path-bin");
        std::fs::create_dir(&installed).unwrap();
        std::fs::create_dir(&path_bin).unwrap();
        for executable in [installed.join("tick"), path_bin.join("tick")] {
            std::fs::write(&executable, "#!/bin/sh\nexit 0\n").unwrap();
            std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let agent = installed.join("tickloop");
        let resolve = |program| {
            resolve_executable_from(
                program,
                directory.path(),
                Some(&agent),
                path_bin.as_os_str(),
            )
            .unwrap()
        };

        assert_eq!(
            resolve("tick"),
            installed.join("tick").canonicalize().unwrap()
        );
        assert_eq!(
            resolve("path-bin/tick"),
            path_bin.join("tick").canonicalize().unwrap()
        );
        std::fs::remove_file(installed.join("tick")).unwrap();
        assert_eq!(
            resolve("tick"),
            path_bin.join("tick").canonicalize().unwrap()
        );
        std::fs::remove_file(path_bin.join("tick")).unwrap();
        assert!(
            resolve_executable_from("tick", directory.path(), Some(&agent), path_bin.as_os_str())
                .is_err()
        );
        std::fs::write(installed.join("tickhost"), "#!/bin/sh\nexit 0\n").unwrap();
        std::fs::set_permissions(
            installed.join("tickhost"),
            std::fs::Permissions::from_mode(0o755),
        )
        .unwrap();
        assert_eq!(
            resolve("tickhost"),
            installed.join("tickhost").canonicalize().unwrap()
        );
    }
}
