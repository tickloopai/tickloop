//! Shared locks retain an immutable executable bundle while a process may use its siblings.

use std::{
    fs::{File, OpenOptions},
    io,
    os::{
        fd::{AsRawFd, FromRawFd, RawFd},
        unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt},
    },
    path::{Path, PathBuf},
};

use sha2::{Digest, Sha256};

pub struct ExecutableLease(File);

impl ExecutableLease {
    pub fn raw_descriptor(&self) -> RawFd {
        self.0.as_raw_fd()
    }

    pub fn make_inheritable(&self) -> io::Result<()> {
        let descriptor = self.raw_descriptor();
        let flags = unsafe { libc::fcntl(descriptor, libc::F_GETFD) };
        if flags < 0
            || unsafe { libc::fcntl(descriptor, libc::F_SETFD, flags & !libc::FD_CLOEXEC) } < 0
        {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    /// Takes a descriptor inherited from exec only after verifying the private lease inode.
    ///
    /// # Safety
    /// The descriptor must have no Rust owner in this process. Success consumes it.
    pub unsafe fn from_inherited(
        descriptor: RawFd,
        directory: &Path,
        leases: &Path,
    ) -> io::Result<Self> {
        if descriptor < 3 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid inherited lease descriptor",
            ));
        }
        let expected = open_lock(&directory.canonicalize()?, leases, false)?.metadata()?;
        let duplicate = unsafe { libc::fcntl(descriptor, libc::F_DUPFD_CLOEXEC, 3) };
        if duplicate < 0 {
            return Err(io::Error::last_os_error());
        }
        let file = unsafe { File::from_raw_fd(duplicate) };
        let actual = file.metadata()?;
        if (actual.dev(), actual.ino()) != (expected.dev(), expected.ino()) {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "inherited descriptor is not the predecessor lease",
            ));
        }
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_SH | libc::LOCK_NB) } != 0 {
            return Err(io::Error::last_os_error());
        }
        unsafe {
            libc::close(descriptor);
        }
        Ok(Self(file))
    }

    pub fn acquire(directory: &Path, leases: &Path) -> io::Result<Self> {
        let directory = directory.canonicalize()?;
        let file = open_lock(&directory, leases, true)?;
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_SH) } != 0 {
            return Err(io::Error::last_os_error());
        }
        if !directory.is_dir() {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "runtime bundle was removed",
            ));
        }
        Ok(Self(file))
    }

    /// No marker means a legacy bundle whose processes do not yet take lifetime leases.
    pub fn try_retire(directory: &Path, leases: &Path) -> io::Result<Option<Self>> {
        let directory = directory.canonicalize()?;
        let file = match open_lock(&directory, leases, false) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error),
        };
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            let error = io::Error::last_os_error();
            return if error.kind() == io::ErrorKind::WouldBlock {
                Ok(None)
            } else {
                Err(error)
            };
        }
        Ok(Some(Self(file)))
    }
}

impl Drop for ExecutableLease {
    fn drop(&mut self) {
        unsafe {
            libc::flock(self.0.as_raw_fd(), libc::LOCK_UN);
        }
    }
}

pub fn lease_directory() -> io::Result<PathBuf> {
    let home = std::env::var_os("HOME")
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "HOME is not set"))?;
    Ok(PathBuf::from(home).join(".local/share/tickloop/runtime-leases"))
}

pub fn retain_current_executable() -> io::Result<Option<ExecutableLease>> {
    let executable = std::env::current_exe()?.canonicalize()?;
    let directory = executable.parent().ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidInput, "executable has no directory")
    })?;
    if !directory.join("installation.json").is_file() {
        return Ok(None);
    }
    ExecutableLease::acquire(directory, &lease_directory()?).map(Some)
}

fn open_lock(directory: &Path, leases: &Path, create: bool) -> io::Result<File> {
    if create {
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(leases)?;
    }
    let metadata = std::fs::symlink_metadata(leases)?;
    if !metadata.is_dir()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.mode() & 0o077 != 0
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "runtime lease directory is not private",
        ));
    }
    let key = format!(
        "{:x}.lock",
        Sha256::digest(directory.as_os_str().as_encoded_bytes())
    );
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(create)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(leases.join(key))?;
    let metadata = file.metadata()?;
    if !metadata.is_file()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.mode() & 0o077 != 0
        || metadata.nlink() != 1
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "runtime lease is not a private file",
        ));
    }
    Ok(file)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retirement_requires_a_registered_bundle_without_live_users() {
        let root = tempfile::tempdir().unwrap();
        let bundle = root.path().join("version");
        std::fs::create_dir(&bundle).unwrap();
        let leases = root.path().join("leases");
        assert!(
            ExecutableLease::try_retire(&bundle, &leases)
                .unwrap()
                .is_none()
        );
        let first = ExecutableLease::acquire(&bundle, &leases).unwrap();
        let second = ExecutableLease::acquire(&bundle, &leases).unwrap();
        assert!(
            ExecutableLease::try_retire(&bundle, &leases)
                .unwrap()
                .is_none()
        );
        drop(first);
        assert!(
            ExecutableLease::try_retire(&bundle, &leases)
                .unwrap()
                .is_none()
        );
        drop(second);
        assert!(
            ExecutableLease::try_retire(&bundle, &leases)
                .unwrap()
                .is_some()
        );
    }

    #[test]
    fn inherited_lease_keeps_the_predecessor_retained_and_is_not_forwarded_to_children() {
        let root = tempfile::tempdir().unwrap();
        let bundle = root.path().join("version");
        std::fs::create_dir(&bundle).unwrap();
        let leases = root.path().join("leases");
        let lease = ExecutableLease::acquire(&bundle, &leases).unwrap();
        lease.make_inheritable().unwrap();
        let descriptor = lease.raw_descriptor();
        std::mem::forget(lease);
        let adopted =
            unsafe { ExecutableLease::from_inherited(descriptor, &bundle, &leases) }.unwrap();
        assert_ne!(
            unsafe { libc::fcntl(adopted.raw_descriptor(), libc::F_GETFD) } & libc::FD_CLOEXEC,
            0
        );
        assert!(
            ExecutableLease::try_retire(&bundle, &leases)
                .unwrap()
                .is_none()
        );
        drop(adopted);
        assert!(
            ExecutableLease::try_retire(&bundle, &leases)
                .unwrap()
                .is_some()
        );
    }
}
