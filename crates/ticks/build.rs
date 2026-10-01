//! Embeds the Longbridge SDK revision Cargo resolved, so the entitlement probe reports the
//! revision it was actually built against instead of a hand-copied one.

use std::{env, fs, path::PathBuf};

const SDK_PACKAGE: &str = "longbridge";

const UNKNOWN_REVISION: &str = "unknown";

fn main() {
    let revision = match workspace_lockfile() {
        Some(lockfile) => {
            println!("cargo::rerun-if-changed={}", lockfile.display());
            match fs::read_to_string(&lockfile) {
                Ok(manifest) => pinned_revision(&manifest).map(str::to_owned),
                Err(error) => {
                    println!(
                        "cargo::warning={} could not be read: {error}",
                        lockfile.display()
                    );
                    None
                }
            }
        }
        None => None,
    };
    let revision = revision.unwrap_or_else(|| {
        println!(
            "cargo::warning=no Cargo.lock pins a git revision for {SDK_PACKAGE}; the probe reports {UNKNOWN_REVISION}"
        );
        UNKNOWN_REVISION.to_owned()
    });
    println!("cargo::rustc-env=LONGBRIDGE_SDK_REVISION={revision}");
}

fn workspace_lockfile() -> Option<PathBuf> {
    let manifest_directory = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR")?);
    manifest_directory
        .ancestors()
        .map(|directory| directory.join("Cargo.lock"))
        .find(|lockfile| lockfile.is_file())
}

/// The `rev=` fragment of the locked git source of the SDK package.
fn pinned_revision(lockfile: &str) -> Option<&str> {
    lockfile
        .split("[[package]]")
        .find(|package| {
            package
                .lines()
                .any(|line| line.trim() == format!("name = \"{SDK_PACKAGE}\""))
        })?
        .lines()
        .find_map(|line| line.trim().strip_prefix("source = \""))?
        .split_once("?rev=")?
        .1
        .split(['#', '"'])
        .next()
}
