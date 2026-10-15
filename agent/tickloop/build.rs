// Derives the build identity a View compares with the running daemon's, and the daemon with its
// Tickhost's. It hashes the workspace sources and the files they embed, so two builds of the same
// sources share it and any change to them yields another. The Tickhost's build script includes
// this file, so plain comments stand in for module documentation.

use std::collections::hash_map::DefaultHasher;
use std::hash::Hasher;
use std::path::{Path, PathBuf};

const SOURCE_DIRECTORIES: [&str; 5] = ["agent", "tick", "tickhost", "ui", "vendor"];
const ROOT_FILES: [&str; 2] = ["Cargo.toml", "Cargo.lock"];
/// Rust sources, manifests and every file type the crates embed with `include_*!`.
const SOURCE_EXTENSIONS: [&str; 9] = [
    "rs", "toml", "md", "json", "yaml", "yml", "pem", "py", "tsv",
];
const SKIPPED_DIRECTORIES: [&str; 2] = ["target", "node_modules"];

fn main() {
    let manifest = PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").expect("cargo sets it"));
    let workspace = manifest.join("../..");
    let mut sources = ROOT_FILES
        .iter()
        .map(|file| workspace.join(file))
        .collect::<Vec<_>>();
    for directory in SOURCE_DIRECTORIES {
        collect_sources(&workspace.join(directory), &mut sources);
    }
    sources.sort();
    let mut hasher = DefaultHasher::new();
    for source in &sources {
        println!("cargo:rerun-if-changed={}", source.display());
        let relative = source.strip_prefix(&workspace).unwrap_or(source);
        hasher.write(relative.to_string_lossy().as_bytes());
        hasher.write(&std::fs::read(source).unwrap_or_default());
    }
    // Only the files themselves are watched: adding or removing a source also edits the module
    // or include that names it, and the directories hold coverage output that changes per run.
    println!(
        "cargo:rustc-env=TICKLOOP_BUILD_IDENTITY={}+{:016x}",
        std::env::var("CARGO_PKG_VERSION").expect("cargo sets it"),
        hasher.finish()
    );
}

fn collect_sources(directory: &Path, sources: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(directory) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.starts_with('.') {
            continue;
        }
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if file_type.is_dir() {
            if !SKIPPED_DIRECTORIES.contains(&name.as_ref()) {
                collect_sources(&path, sources);
            }
        } else if file_type.is_file()
            && path
                .extension()
                .and_then(|extension| extension.to_str())
                .is_some_and(|extension| SOURCE_EXTENSIONS.contains(&extension))
        {
            sources.push(path);
        }
    }
}
