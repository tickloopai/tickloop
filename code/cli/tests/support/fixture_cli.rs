//! Internal transport fixture executable; the shipped tickloop binary never reads these files.
use std::{
    any::TypeId,
    os::unix::{fs::MetadataExt, process::ExitStatusExt},
    path::{Path, PathBuf},
    sync::OnceLock,
};

use tickloop_code_local_environment::hosted::controller::TICKHOST_EXECUTABLE;

/// The fixture's name as the product CLI; as [`TICKHOST_EXECUTABLE`] it runs the Tickhost.
const TICKLOOP_EXECUTABLE: &str = "tickloop";

const BUNDLE_NAMES: [&str; 2] = [TICKLOOP_EXECUTABLE, TICKHOST_EXECUTABLE];

/// Serializes bundle installs in one build directory; the name stays outside the
/// `cli_fixture-` candidates.
const BUNDLE_LOCK_FILE: &str = "cli_fixture.bundle.lock";

/// The fixture's first argument that runs the product CLI; any other unknown first argument is
/// taken as a libtest probe.
pub const CLI_MODE: &str = "--cli";

/// The fixture's first argument that prints its [`build_identity`] and exits.
pub const BUILD_IDENTITY_MODE: &str = "--build-identity";

/// Names the compiled crates this executable links. A type's identity derives from its crate's
/// Cargo metadata, which a build with another package selection, feature set or profile changes,
/// so a test binary and a fixture agree only when one Cargo build produced both. Each library
/// the executable links covers its own dependencies; the dev-dependencies that export a type are
/// listed one by one.
pub fn build_identity() -> String {
    format!(
        "{:?}",
        [
            TypeId::of::<tickloop_code_client::LocalAgentClient>(),
            TypeId::of::<tickloop_code_console::agent_console::AgentConsole>(),
            TypeId::of::<tickloop_code_daemon::EmbeddedAgentAccess>(),
            TypeId::of::<percent_encoding::AsciiSet>(),
            TypeId::of::<portable_pty::CommandBuilder>(),
            TypeId::of::<serde_yaml_ng::Value>(),
            TypeId::of::<tickloop_harness_test_support::ScriptedModel>(),
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
    BUNDLE.get_or_init(install_bundle)
}

/// Finds the fixture and copies it into `<fixture>.bundle`, replacing copies that Cargo's
/// in-place rebuild of the fixture left behind.
///
/// Each bundle name gets its own file because the product canonicalizes these paths and picks
/// the process by file name. Hard links are no option: macOS assesses a freshly linked executable
/// on its first exec, and a first exec through a second name of the file is SIGKILLed now and
/// then while other processes run it through another. On APFS the copies are clones. Each copy
/// reports its build identity before it counts as installed, so its own first exec, which macOS
/// slows by seconds, happens here rather than inside a test's time budget.
fn install_bundle() -> PathBuf {
    let directory = std::env::current_exe()
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf();
    let fixture = find_executable(&directory);
    let bundle = fixture.with_extension("bundle");
    if is_bundle_of(&bundle, &fixture) {
        return bundle;
    }
    let lock = std::fs::File::create(directory.join(BUNDLE_LOCK_FILE)).unwrap();
    lock.lock().unwrap();
    if !is_bundle_of(&bundle, &fixture) {
        if bundle.exists() {
            std::fs::remove_dir_all(&bundle).unwrap();
        }
        std::fs::create_dir(&bundle).unwrap();
        let built = std::fs::metadata(&fixture).unwrap().modified().unwrap();
        let copies = BUNDLE_NAMES.map(|name| bundle.join(name));
        for copy in &copies {
            std::fs::copy(&fixture, copy).unwrap();
        }
        // First execs of separate files are assessed concurrently.
        std::thread::scope(|scope| {
            for copy in &copies {
                scope.spawn(|| {
                    assert_eq!(
                        reported_build_identity(copy),
                        Some(build_identity()),
                        "{} is not a runnable copy of {}",
                        copy.display(),
                        fixture.display()
                    );
                });
            }
        });
        // The time marks a complete, verified copy of this build.
        for copy in &copies {
            std::fs::File::open(copy)
                .unwrap()
                .set_modified(built)
                .unwrap();
        }
    }
    bundle
}

/// Whether every bundle name is a file of its own, neither a link nor a second name of the
/// fixture, copied from the fixture's current build.
fn is_bundle_of(bundle: &Path, fixture: &Path) -> bool {
    let build = |metadata: std::fs::Metadata| Some((metadata.len(), metadata.modified().ok()?));
    let Some(fixture_build) = std::fs::metadata(fixture).ok().and_then(build) else {
        return false;
    };
    BUNDLE_NAMES.iter().all(|name| {
        std::fs::symlink_metadata(bundle.join(name))
            .ok()
            .filter(|metadata| metadata.is_file() && metadata.nlink() == 1)
            .and_then(build)
            == Some(fixture_build)
    })
}

/// The `cli_fixture` built together with this test binary. Builds with another package selection
/// leave fixtures under other hashes in the same directory, so each candidate is asked for its
/// build identity instead of trusting file names or times.
fn find_executable(directory: &Path) -> PathBuf {
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

/// `None` when the executable cannot start or predates the identity mode. A probe killed by a
/// signal says nothing about the candidate, so it fails the test instead of skipping it.
fn reported_build_identity(executable: &Path) -> Option<String> {
    let output = std::process::Command::new(executable)
        .arg(BUILD_IDENTITY_MODE)
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    if let Some(signal) = output.status.signal() {
        panic!(
            "{} {BUILD_IDENTITY_MODE} was killed by signal {signal}",
            executable.display()
        );
    }
    let reported = String::from_utf8(output.stdout).ok()?;
    Some(reported.trim_end().to_owned())
}

/// The default model's key for every fixture CLI, so no test reads the developer's real key;
/// a test that needs it unset removes it.
const FIXTURE_API_KEY: (&str, &str) = ("DEEPSEEK_API_KEY", "fixture-default-key");

/// The product CLI through the fixture; callers append CLI arguments.
pub fn command() -> std::process::Command {
    let mut command = std::process::Command::new(executable());
    command
        .arg(CLI_MODE)
        .env(FIXTURE_API_KEY.0, FIXTURE_API_KEY.1);
    command
}

pub fn tokio_command() -> tokio::process::Command {
    let mut command = tokio::process::Command::new(executable());
    command
        .arg(CLI_MODE)
        .env(FIXTURE_API_KEY.0, FIXTURE_API_KEY.1);
    command
}

pub fn pty_command() -> portable_pty::CommandBuilder {
    let mut command = portable_pty::CommandBuilder::new(executable());
    command.arg(CLI_MODE);
    command.env(FIXTURE_API_KEY.0, FIXTURE_API_KEY.1);
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
