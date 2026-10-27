//! Git worktree links as `git worktree add` writes them, verified in both directions so a
//! repository's own files cannot claim a directory elsewhere.

use std::fs;
use std::path::{Component, Path, PathBuf};

const GIT_ENTRY: &str = ".git";
const WORKTREES_DIRECTORY: &str = "worktrees";
/// Bounds the directories a repository can add to the permission scope.
const MAX_LINKED_WORKTREES: usize = 32;

/// The canonical common git directory of the linked worktree checked out at `worktree_root`:
/// its `.git` file names an entry under `<common>/worktrees/`, and that entry links back.
pub fn linked_common_directory(worktree_root: &Path) -> Option<PathBuf> {
    let pointer = fs::read_to_string(worktree_root.join(GIT_ENTRY)).ok()?;
    let worktree_git_directory =
        normalize(&worktree_root.join(pointer.trim().strip_prefix("gitdir:")?.trim()));
    let common_directory = normalize(
        &worktree_git_directory.join(
            fs::read_to_string(worktree_git_directory.join("commondir"))
                .ok()?
                .trim(),
        ),
    );
    if worktree_git_directory.parent()? != common_directory.join(WORKTREES_DIRECTORY) {
        return None;
    }
    let backlink = fs::read_to_string(worktree_git_directory.join("gitdir")).ok()?;
    let backlink = fs::canonicalize(worktree_git_directory.join(backlink.trim())).ok()?;
    if backlink != fs::canonicalize(worktree_root).ok()?.join(GIT_ENTRY) {
        return None;
    }
    fs::canonicalize(common_directory).ok()
}

/// The main working directory of the linked worktree at `worktree_root`, or its common git
/// directory when the repository is bare.
pub fn main_repository_root(worktree_root: &Path) -> Option<PathBuf> {
    let common_directory = linked_common_directory(worktree_root)?;
    if common_directory.file_name()? != GIT_ENTRY {
        return Some(common_directory);
    }
    common_directory.parent().map(Path::to_path_buf)
}

/// The canonical roots of the other linked worktrees registered in the repository holding
/// `directory`, each verified by [`linked_common_directory`]. Worktrees enclosing `directory`
/// or the repository's git directory are left out, so none can widen the scope to an ancestor.
pub fn sibling_linked_worktrees(directory: &Path) -> Vec<PathBuf> {
    let Ok(directory) = fs::canonicalize(directory) else {
        return Vec::new();
    };
    let Some(common_directory) = common_directory_of(&directory) else {
        return Vec::new();
    };
    let Ok(entries) = fs::read_dir(common_directory.join(WORKTREES_DIRECTORY)) else {
        return Vec::new();
    };
    let mut worktrees = entries
        .flatten()
        .filter_map(|entry| {
            let backlink = fs::read_to_string(entry.path().join("gitdir")).ok()?;
            let root = fs::canonicalize(entry.path().join(backlink.trim()).parent()?).ok()?;
            (linked_common_directory(&root)? == common_directory).then_some(root)
        })
        .filter(|root| !directory.starts_with(root) && !common_directory.starts_with(root))
        .collect::<Vec<_>>();
    worktrees.sort();
    worktrees.truncate(MAX_LINKED_WORKTREES);
    worktrees
}

fn common_directory_of(directory: &Path) -> Option<PathBuf> {
    let repository_root = directory
        .ancestors()
        .find(|ancestor| ancestor.join(GIT_ENTRY).exists())?;
    let git_entry = repository_root.join(GIT_ENTRY);
    if git_entry.is_dir() {
        return fs::canonicalize(git_entry).ok();
    }
    linked_common_directory(repository_root)
}

fn normalize(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::ParentDir => {
                normalized.pop();
            }
            Component::CurDir => {}
            component => normalized.push(component),
        }
    }
    normalized
}

#[cfg(test)]
#[path = "git_worktree_tests.rs"]
mod tests;
