use std::os::unix::fs::MetadataExt;
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    time::SystemTime,
};

#[derive(Default)]
pub(crate) struct ConfigurationFiles {
    pub paths: Vec<PathBuf>,
    previous: Option<BTreeMap<PathBuf, Stamp>>,
}
#[derive(PartialEq, Eq)]
struct Stamp {
    modified: Option<SystemTime>,
    len: u64,
    inode: u64,
    mode: u32,
}
impl ConfigurationFiles {
    fn revision(&self, root: &Path) -> BTreeMap<PathBuf, Stamp> {
        let mut paths = self.paths.clone();
        paths.extend([
            root.join("settings.json"),
            root.join("plugins/installed.json"),
        ]);
        credential_paths(&root.join("credentials"), 3, &mut paths);
        paths
            .into_iter()
            .filter_map(|path| {
                fs::symlink_metadata(&path).ok().map(|metadata| {
                    (
                        path,
                        Stamp {
                            modified: metadata.modified().ok(),
                            len: metadata.len(),
                            inode: metadata.ino(),
                            mode: metadata.mode(),
                        },
                    )
                })
            })
            .collect()
    }
    pub fn unchanged(&self, root: &Path) -> bool {
        self.previous
            .as_ref()
            .is_some_and(|previous| *previous == self.revision(root))
    }
    pub fn remember(&mut self, root: &Path) {
        self.previous = Some(self.revision(root));
    }
}
fn credential_paths(directory: &Path, depth: usize, paths: &mut Vec<PathBuf>) {
    if depth == 0 {
        return;
    }
    if let Ok(entries) = fs::read_dir(directory) {
        for entry in entries.flatten() {
            if entry.file_type().is_ok_and(|kind| kind.is_dir()) {
                credential_paths(&entry.path(), depth - 1, paths);
            } else if entry
                .path()
                .extension()
                .is_some_and(|extension| extension == "json")
            {
                paths.push(entry.path());
            }
        }
    }
}
