//! The Tickhost receives descriptors through SCM_RIGHTS, which outside Linux arrive without
//! close-on-exec until marked. Whether a child forked in that window inherits one is a race of a
//! few instructions that no test makes happen on demand, so this checks the structure that rules
//! it out: every child the tickhost crates start is spawned under `hosted::spawn_child`, which
//! excludes a descriptor arriving meanwhile.

use std::path::{Path, PathBuf};

/// `Command` methods that start a child process.
const CHILD_STARTS: [&str; 2] = [".spawn()", ".output()"];
/// Ways of starting a process that bypass `Command`.
const RAW_STARTS: [&str; 3] = ["libc::fork", "posix_spawn", "libc::execv"];
const GUARD: &str = "spawn_child(";

fn sources(directory: &Path, found: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(directory).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            sources(&path, found);
        } else if path.extension().is_some_and(|extension| extension == "rs") {
            found.push(path);
        }
    }
}

/// A source file up to its unit test module, which runs in the test process only.
fn production_lines(path: &Path) -> Vec<(usize, String)> {
    std::fs::read_to_string(path)
        .unwrap()
        .lines()
        .take_while(|line| *line != "#[cfg(test)]")
        .enumerate()
        .map(|(index, line)| (index + 1, line.to_owned()))
        .collect()
}

#[test]
fn every_child_the_tickhost_crates_start_is_spawned_under_the_descriptor_guard() {
    let host = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut files = Vec::new();
    sources(&host.join("src"), &mut files);
    sources(&host.join("../connect/src"), &mut files);
    let mut guarded = 0;
    let mut unguarded = Vec::new();
    for file in &files {
        for (number, line) in production_lines(file) {
            if RAW_STARTS.iter().any(|start| line.contains(start)) {
                unguarded.push(format!("{}:{number}: {}", file.display(), line.trim()));
            } else if CHILD_STARTS.iter().any(|start| line.contains(start)) {
                if line.contains(GUARD) {
                    guarded += 1;
                } else {
                    unguarded.push(format!("{}:{number}: {}", file.display(), line.trim()));
                }
            }
        }
    }
    assert!(
        unguarded.is_empty(),
        "children started outside hosted::spawn_child:\n{}",
        unguarded.join("\n")
    );
    // A Shell task, a hosted Tick's launcher or terminal program, and a launcher's business Tick.
    assert_eq!(guarded, 3, "the spawn paths changed; check each is guarded");
}
