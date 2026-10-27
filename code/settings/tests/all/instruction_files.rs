use std::{
    fs,
    path::{Path, PathBuf},
};
use tickloop_code_settings::instructions::instruction_files;
use tickloop_harness::policy::instructions::{InstructionFile, InstructionScope};

struct Machine {
    _directory: tempfile::TempDir,
    root: PathBuf,
}

impl Machine {
    fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        fs::create_dir_all(root.join("home/.tickloop")).unwrap();
        Self {
            _directory: directory,
            root,
        }
    }

    fn path(&self, relative: &str) -> PathBuf {
        self.root.join(relative)
    }

    fn write(&self, relative: &str, content: &str) {
        let path = self.path(relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, content).unwrap();
    }

    fn files(&self, folder: Option<&str>) -> Vec<InstructionFile> {
        let folder = folder.map(|folder| {
            fs::create_dir_all(self.path(folder)).unwrap();
            self.path(folder)
        });
        instruction_files(
            folder.as_deref(),
            Some(&self.path("home/.tickloop")),
            Some(&self.path("home")),
        )
    }

    /// The loaded files as paths relative to the machine root with their scope.
    fn loaded(&self, folder: Option<&str>) -> Vec<(String, InstructionScope)> {
        self.files(folder)
            .into_iter()
            .map(|file| (self.relative(&file.path), file.scope))
            .collect()
    }

    fn relative(&self, path: &Path) -> String {
        path.strip_prefix(&self.root)
            .unwrap()
            .to_string_lossy()
            .into_owned()
    }
}

use InstructionScope::{Local, Project, User};

fn entries(expected: &[(&str, InstructionScope)]) -> Vec<(String, InstructionScope)> {
    expected
        .iter()
        .map(|(path, scope)| ((*path).to_owned(), *scope))
        .collect()
}

#[test]
fn each_directory_uses_agents_md_or_else_its_claude_files_nearest_last() {
    let machine = Machine::new();
    machine.write("home/work/AGENTS.md", "Series rules.");
    machine.write("home/work/repo/AGENTS.md", "Repository rules.");
    machine.write("home/work/repo/CLAUDE.md", "Ignored beside AGENTS.md.");
    machine.write("home/work/repo/.claude/CLAUDE.md", "Also ignored.");
    machine.write("home/work/repo/CLAUDE.local.md", "My private notes.");
    machine.write("home/work/repo/pkg/CLAUDE.md", "Package rules.");
    machine.write(
        "home/work/repo/pkg/.claude/CLAUDE.md",
        "Package Claude rules.",
    );
    machine.write("home/work/repo/pkg/.claude/rules/b.md", "Rule b.");
    machine.write(
        "home/work/repo/pkg/.claude/rules/nested/a.md",
        "Nested rule.",
    );
    machine.write(
        "home/work/repo/pkg/.claude/rules/scoped.md",
        "---\npaths: src/**/*.ts\n---\nOnly for TypeScript.",
    );
    machine.write("home/work/repo/pkg/deeper/CLAUDE.md", "Below the Folder.");

    assert_eq!(
        machine.loaded(Some("home/work/repo/pkg")),
        entries(&[
            ("home/work/AGENTS.md", Project),
            ("home/work/repo/AGENTS.md", Project),
            ("home/work/repo/CLAUDE.local.md", Local),
            ("home/work/repo/pkg/CLAUDE.md", Project),
            ("home/work/repo/pkg/.claude/CLAUDE.md", Project),
            ("home/work/repo/pkg/.claude/rules/b.md", Project),
            ("home/work/repo/pkg/.claude/rules/nested/a.md", Project),
        ])
    );
}

#[test]
fn an_empty_agents_md_keeps_its_directory_from_falling_back() {
    let machine = Machine::new();
    machine.write("home/repo/AGENTS.md", "");
    machine.write("home/repo/CLAUDE.md", "Claude rules.");
    machine.write("home/repo/CLAUDE.local.md", "Still mine.");

    assert_eq!(
        machine.loaded(Some("home/repo")),
        entries(&[("home/repo/CLAUDE.local.md", Local)])
    );
}

#[test]
fn user_instructions_fall_back_to_claude_code_and_load_once() {
    let machine = Machine::new();
    machine.write("home/.claude/CLAUDE.md", "Claude global.");
    machine.write("home/.claude/rules/style.md", "Claude global rule.");
    machine.write("home/CLAUDE.md", "Home project file.");
    machine.write("home/repo/AGENTS.md", "Repository rules.");

    assert_eq!(
        machine.loaded(Some("home/repo")),
        entries(&[
            ("home/.claude/CLAUDE.md", User),
            ("home/.claude/rules/style.md", User),
            ("home/CLAUDE.md", Project),
            ("home/repo/AGENTS.md", Project),
        ])
    );

    machine.write("home/.tickloop/AGENTS.md", "Tickloop global.");
    assert_eq!(
        machine.loaded(Some("home/repo")),
        entries(&[
            ("home/.tickloop/AGENTS.md", User),
            ("home/CLAUDE.md", Project),
            ("home/repo/AGENTS.md", Project),
        ])
    );
    assert_eq!(
        machine.loaded(None),
        entries(&[("home/.tickloop/AGENTS.md", User)])
    );
}

#[test]
fn imports_follow_their_file_once_and_stay_inside_the_folder_for_project_files() {
    let machine = Machine::new();
    machine.write(
        "home/repo/AGENTS.md",
        "Read @docs/a.md and @docs/a.md again, @../outside.md, @docs/image.png\n```\n@docs/fenced.md\n```",
    );
    machine.write("home/repo/docs/a.md", "A imports @b.md and @../AGENTS.md.");
    machine.write("home/repo/docs/b.md", "B.");
    machine.write("home/repo/docs/fenced.md", "Never imported.");
    machine.write("home/repo/docs/image.png", "not text");
    machine.write("home/outside.md", "Outside the Folder.");
    machine.write("home/.tickloop/AGENTS.md", "Global, see @~/outside.md");

    let files = machine.files(Some("home/repo"));
    assert_eq!(
        files
            .iter()
            .map(|file| (machine.relative(&file.path), file.scope))
            .collect::<Vec<_>>(),
        entries(&[
            ("home/.tickloop/AGENTS.md", User),
            ("home/outside.md", User),
            ("home/repo/AGENTS.md", Project),
            ("home/repo/docs/a.md", Project),
            ("home/repo/docs/b.md", Project),
        ])
    );
}

#[test]
fn imports_stop_five_levels_deep() {
    let machine = Machine::new();
    machine.write("home/repo/AGENTS.md", "@1.md");
    for level in 1..=6 {
        machine.write(
            &format!("home/repo/{level}.md"),
            &format!("Level {level} @{}.md", level + 1),
        );
    }

    let loaded = machine.loaded(Some("home/repo"));
    assert_eq!(
        loaded
            .iter()
            .map(|(path, _)| path.as_str())
            .collect::<Vec<_>>(),
        [
            "home/repo/AGENTS.md",
            "home/repo/1.md",
            "home/repo/2.md",
            "home/repo/3.md",
            "home/repo/4.md",
        ]
    );
}

#[test]
fn content_drops_frontmatter_and_block_comments() {
    let machine = Machine::new();
    machine.write(
        "home/repo/AGENTS.md",
        "---\ndescription: rules\n---\nKeep this.\n\n<!-- maintainer note -->\n\nAnd this.",
    );

    let files = machine.files(Some("home/repo"));
    assert_eq!(files[0].content, "Keep this.\n\nAnd this.");
}

#[test]
#[cfg(unix)]
fn a_linked_file_is_loaded_once() {
    let machine = Machine::new();
    machine.write("home/repo/AGENTS.md", "Shared rules.");
    std::os::unix::fs::symlink(
        machine.path("home/repo/AGENTS.md"),
        machine.path("home/repo/CLAUDE.local.md"),
    )
    .unwrap();

    assert_eq!(
        machine.loaded(Some("home/repo")),
        entries(&[("home/repo/AGENTS.md", Project)])
    );
}

#[test]
fn a_worktree_inside_its_repository_skips_the_main_checked_in_files() {
    let machine = Machine::new();
    let main = machine.path("home/main");
    let worktree = main.join(".claude/worktrees/feature");
    let link = main.join(".git/worktrees/feature");
    fs::create_dir_all(&link).unwrap();
    fs::create_dir_all(&worktree).unwrap();
    fs::write(link.join("commondir"), "../..\n").unwrap();
    fs::write(
        link.join("gitdir"),
        format!("{}\n", worktree.join(".git").display()),
    )
    .unwrap();
    fs::write(
        worktree.join(".git"),
        format!("gitdir: {}\n", link.display()),
    )
    .unwrap();
    machine.write("home/main/AGENTS.md", "Main checkout.");
    machine.write("home/main/CLAUDE.local.md", "Private notes kept.");
    machine.write("home/main/.claude/worktrees/feature/AGENTS.md", "Worktree.");

    assert_eq!(
        machine.loaded(Some("home/main/.claude/worktrees/feature")),
        entries(&[
            ("home/main/CLAUDE.local.md", Local),
            ("home/main/.claude/worktrees/feature/AGENTS.md", Project),
        ])
    );
}

#[test]
#[cfg(unix)]
fn a_project_import_linked_outside_the_folder_is_not_loaded() {
    let machine = Machine::new();
    machine.write("home/secret.md", "Outside the Folder.");
    machine.write("home/repo/AGENTS.md", "See @notes.md and @docs/inside.md");
    machine.write("home/repo/real/inside.md", "Inside the Folder.");
    std::os::unix::fs::symlink(
        machine.path("home/secret.md"),
        machine.path("home/repo/notes.md"),
    )
    .unwrap();
    std::os::unix::fs::symlink(
        machine.path("home/repo/real"),
        machine.path("home/repo/docs"),
    )
    .unwrap();

    assert_eq!(
        machine.loaded(Some("home/repo")),
        entries(&[
            ("home/repo/AGENTS.md", Project),
            ("home/repo/docs/inside.md", Project),
        ])
    );
}

#[test]
#[cfg(unix)]
fn a_folder_reached_through_a_link_keeps_its_imports() {
    let machine = Machine::new();
    machine.write("home/repo/AGENTS.md", "See @docs/a.md");
    machine.write("home/repo/docs/a.md", "Imported.");
    std::os::unix::fs::symlink(machine.path("home/repo"), machine.path("linked")).unwrap();

    let files = instruction_files(Some(&machine.path("linked")), None, None);
    assert_eq!(
        files
            .iter()
            .map(|file| machine.relative(&file.path))
            .collect::<Vec<_>>(),
        ["home/repo/AGENTS.md", "home/repo/docs/a.md"]
    );
}
