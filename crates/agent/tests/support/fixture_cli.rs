//! Internal transport fixture executable; the shipped tickloop binary never reads these files.
use std::{
    any::TypeId,
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
    sync::OnceLock,
};

use tickloop_agent::hosted::controller::TICKHOST_EXECUTABLE;

/// The fixture's name as the product CLI; as [`TICKHOST_EXECUTABLE`] it runs the Tickhost.
const TICKLOOP_EXECUTABLE: &str = "tickloop";

/// The fixture's first argument that runs the product CLI; any other unknown first argument is
/// taken as a libtest probe.
pub const CLI_MODE: &str = "--cli";

/// The fixture's first argument that prints its [`build_identity`] and exits.
pub const BUILD_IDENTITY_MODE: &str = "--build-identity";

/// Names the compiled library crates this executable links. A type's identity derives from its
/// crate's Cargo metadata, which a build with another package selection, feature set or profile
/// changes, so a test binary and a fixture agree only when one Cargo build produced both. The
/// library covers its own dependencies; the dev-dependencies are listed one by one.
pub fn build_identity() -> String {
    format!(
        "{:?}",
        [
            TypeId::of::<tickloop_agent_protocol::AccessLimits>(),
            TypeId::of::<portable_pty::CommandBuilder>(),
            TypeId::of::<vt100::Parser>(),
        ]
    )
}

/// The fixture executable, for callers that pass a product process mode themselves.
pub fn executable() -> PathBuf {
    bundle().join(TICKLOOP_EXECUTABLE)
}

/// The fixture as the `tickhost` executable next to [`executable`], where a daemon looks for it.
pub fn tickhost() -> PathBuf {
    bundle().join(TICKHOST_EXECUTABLE)
}

/// A directory holding the fixture as `tickloop` and as `tickhost`, side by side the way the
/// product installs both, so a daemon the fixture starts runs a Tickhost of the same build.
fn bundle() -> &'static Path {
    // `deps` can hold over 100k build artifacts; scanning it once per spawned process made tests
    // that start many Ticks CPU-bound.
    static BUNDLE: OnceLock<PathBuf> = OnceLock::new();
    BUNDLE.get_or_init(|| install_bundle(&find_executable()))
}

/// Hard-links the fixture into `<fixture>.bundle`. Cargo rebuilds a fixture in place, so a
/// bundle whose links no longer name the fixture's file is replaced. Concurrent tests stage
/// their own copy and publish it with one rename.
fn install_bundle(fixture: &Path) -> PathBuf {
    let bundle = fixture.with_extension("bundle");
    let names = [TICKLOOP_EXECUTABLE, TICKHOST_EXECUTABLE];
    let identity = |path: &Path| {
        std::fs::metadata(path)
            .ok()
            .map(|metadata| (metadata.dev(), metadata.ino()))
    };
    let fixture_identity = identity(fixture);
    let is_current = |directory: &Path| {
        names
            .iter()
            .all(|name| identity(&directory.join(name)) == fixture_identity)
    };
    if is_current(&bundle) {
        return bundle;
    }
    let process = std::process::id();
    let staging = fixture.with_extension(format!("bundle.{process}"));
    let _ = std::fs::remove_dir_all(&staging);
    std::fs::create_dir(&staging).unwrap();
    for name in names {
        std::fs::hard_link(fixture, staging.join(name)).unwrap();
    }
    if bundle.exists() && !is_current(&bundle) {
        let stale = fixture.with_extension(format!("bundle.stale.{process}"));
        if std::fs::rename(&bundle, &stale).is_ok() {
            let _ = std::fs::remove_dir_all(stale);
        }
    }
    if std::fs::rename(&staging, &bundle).is_err() {
        let _ = std::fs::remove_dir_all(&staging);
    }
    assert!(
        is_current(&bundle),
        "{} names another fixture",
        bundle.display()
    );
    bundle
}

/// The `cli_fixture` built together with this test binary. Builds with another package selection
/// leave fixtures under other hashes in the same directory, so each candidate is asked for its
/// build identity instead of trusting file names or times.
fn find_executable() -> PathBuf {
    let directory = std::env::current_exe()
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf();
    let identity = build_identity();
    std::fs::read_dir(directory)
        .unwrap()
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("cli_fixture-")
                && path.extension().is_none()
                && path.is_file()
        })
        .find(|path| reported_build_identity(path).is_some_and(|reported| reported == identity))
        .expect(
            "build the transport fixture in the same cargo invocation as this test binary, \
             e.g. cargo test --test cli_fixture --test all --no-run",
        )
}

/// `None` when the executable cannot run or predates the identity mode.
fn reported_build_identity(executable: &std::path::Path) -> Option<String> {
    let output = std::process::Command::new(executable)
        .arg(BUILD_IDENTITY_MODE)
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    let reported = String::from_utf8(output.stdout).ok()?;
    Some(reported.trim_end().to_owned())
}

/// The product CLI through the fixture; callers append CLI arguments.
pub fn command() -> std::process::Command {
    let mut command = std::process::Command::new(executable());
    command.arg(CLI_MODE);
    command
}

pub fn tokio_command() -> tokio::process::Command {
    let mut command = tokio::process::Command::new(executable());
    command.arg(CLI_MODE);
    command
}

pub fn pty_command() -> portable_pty::CommandBuilder {
    let mut command = portable_pty::CommandBuilder::new(executable());
    command.arg(CLI_MODE);
    command
}

#[test]
fn the_fixture_comes_from_the_build_of_this_test_binary() {
    let output = std::process::Command::new(executable())
        .arg(BUILD_IDENTITY_MODE)
        .output()
        .unwrap();
    assert_eq!(
        String::from_utf8(output.stdout).unwrap().trim_end(),
        build_identity()
    );
}
