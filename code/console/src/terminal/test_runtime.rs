use std::path::{Path, PathBuf};

pub(super) struct RuntimeDirectories {
    daemon: PathBuf,
    tickhost: PathBuf,
}

impl RuntimeDirectories {
    pub(super) fn of_config_root(config_root: &Path) -> Self {
        std::fs::create_dir_all(config_root).unwrap();
        Self {
            daemon: tickloop_code_client::daemon_endpoint::runtime_directory_of(Some(config_root)),
            tickhost: tickhost::paths::HostPaths::of_config_root(Some(config_root)).runtime,
        }
    }
}

impl Drop for RuntimeDirectories {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.daemon);
        let _ = std::fs::remove_dir_all(&self.tickhost);
    }
}
