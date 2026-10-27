use super::*;
use std::process::Command;

fn git(directory: &Path, arguments: &[&str]) {
    let status = Command::new("git")
        .args(["-c", "user.name=Test", "-c", "user.email=test@example.com"])
        .args(arguments)
        .current_dir(directory)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .status()
        .unwrap();
    assert!(status.success(), "git {arguments:?} failed");
}

/// A repository at `<root>/main` with linked worktrees at `<root>/worktrees/<name>`.
fn repository_with_worktrees(names: &[&str]) -> (tempfile::TempDir, PathBuf) {
    let root = tempfile::tempdir().unwrap();
    let main = root.path().join("main");
    fs::create_dir(&main).unwrap();
    git(&main, &["init", "--quiet"]);
    git(&main, &["commit", "--quiet", "--allow-empty", "-m", "init"]);
    for name in names {
        let path = root.path().join("worktrees").join(name);
        git(
            &main,
            &[
                "worktree",
                "add",
                "--quiet",
                "-b",
                name,
                path.to_str().unwrap(),
            ],
        );
    }
    let root_path = fs::canonicalize(root.path()).unwrap();
    (root, root_path)
}

#[test]
fn a_linked_worktree_resolves_its_main_checkout() {
    let (_guard, root) = repository_with_worktrees(&["feature"]);

    assert_eq!(
        main_repository_root(&root.join("worktrees/feature")),
        Some(root.join("main"))
    );
    assert_eq!(main_repository_root(&root.join("main")), None);
}

#[test]
fn siblings_are_listed_from_a_linked_worktree_and_from_the_main_checkout() {
    let (_guard, root) = repository_with_worktrees(&["first", "second"]);
    let nested = root.join("worktrees/first/nested");
    fs::create_dir(&nested).unwrap();

    assert_eq!(
        sibling_linked_worktrees(&nested),
        vec![root.join("worktrees/second")],
        "the worktree holding the directory is already in scope and is not repeated"
    );
    assert_eq!(
        sibling_linked_worktrees(&root.join("main")),
        vec![root.join("worktrees/first"), root.join("worktrees/second")]
    );
}

#[test]
fn a_worktree_entry_whose_checkout_does_not_link_back_is_ignored() {
    let (_guard, root) = repository_with_worktrees(&["real"]);
    let elsewhere = root.join("elsewhere");
    fs::create_dir(&elsewhere).unwrap();
    let forged = root.join("main/.git/worktrees/forged");
    fs::create_dir(&forged).unwrap();
    fs::write(
        forged.join("gitdir"),
        format!("{}\n", elsewhere.join(".git").display()),
    )
    .unwrap();
    fs::write(forged.join("commondir"), "../..\n").unwrap();

    assert_eq!(
        sibling_linked_worktrees(&root.join("main")),
        vec![root.join("worktrees/real")]
    );
}

#[test]
fn a_worktree_enclosing_the_repository_never_widens_the_scope() {
    let (_guard, root) = repository_with_worktrees(&[]);
    let enclosing = root.join("main/.git/worktrees/enclosing");
    fs::create_dir_all(&enclosing).unwrap();
    fs::write(
        enclosing.join("gitdir"),
        format!("{}\n", root.join(".git").display()),
    )
    .unwrap();
    fs::write(enclosing.join("commondir"), "../..\n").unwrap();
    fs::write(
        root.join(".git"),
        format!("gitdir: {}\n", enclosing.display()),
    )
    .unwrap();

    assert_eq!(linked_common_directory(&root), Some(root.join("main/.git")));
    assert!(sibling_linked_worktrees(&root.join("main")).is_empty());
}

#[test]
fn a_directory_outside_any_repository_has_no_siblings() {
    let directory = tempfile::tempdir().unwrap();

    assert!(sibling_linked_worktrees(directory.path()).is_empty());
}
