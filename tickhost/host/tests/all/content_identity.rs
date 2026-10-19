#[path = "../../content_identity.rs"]
mod fingerprint_support;

use std::collections::BTreeMap;
use std::path::Path;

fn write(root: &Path, name: &str, text: &str) {
    let path = root.join(name);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

fn fixture() -> tempfile::TempDir {
    let root = tempfile::tempdir().unwrap();
    write(
        root.path(),
        "Cargo.toml",
        "[workspace]\nresolver = '3'\n[workspace.package]\nversion = '0.2.8'\nedition = '2024'\n[profile.release]\nlto = true\n",
    );
    write(
        root.path(),
        "Cargo.lock",
        r#"
version = 4
[[package]]
name = "host"
version = "0.2.8"
dependencies = ["protocol", "external", "test-only"]
[[package]]
name = "protocol"
version = "0.2.8"
[[package]]
name = "external"
version = "1.0.0"
source = "registry+https://example.test/index"
checksum = "aaaa"
dependencies = ["transitive"]
[[package]]
name = "transitive"
version = "1.0.0"
source = "registry+https://example.test/index"
checksum = "bbbb"
[[package]]
name = "test-only"
version = "1.0.0"
source = "registry+https://example.test/index"
checksum = "cccc"
[[package]]
name = "unrelated"
version = "1.0.0"
source = "registry+https://example.test/index"
checksum = "dddd"
"#,
    );
    write(
        root.path(),
        "host/Cargo.toml",
        "[package]\nname = 'host'\nversion.workspace = true\nedition.workspace = true\n[dependencies]\nprotocol = { path = '../protocol' }\nexternal = { version = '1', features = ['runtime'] }\n[dev-dependencies]\ntest-only = '1'\n",
    );
    write(
        root.path(),
        "protocol/Cargo.toml",
        "[package]\nname = 'protocol'\nversion.workspace = true\nedition.workspace = true\n",
    );
    write(root.path(), "host/src/main.rs", "fn main() {}\n");
    write(root.path(), "host/build.rs", "fn main() {}\n");
    write(
        root.path(),
        "protocol/src/lib.rs",
        "pub const MAJOR: u16 = 1;\n",
    );
    write(root.path(), "agent/src/main.rs", "fn main() {}\n");
    root
}

fn identity(root: &Path) -> String {
    let result =
        fingerprint_support::fingerprint(root, &root.join("host"), &BTreeMap::new()).unwrap();
    let canonical = root.canonicalize().unwrap();
    assert!(result.inputs.contains(&canonical.join("Cargo.lock")));
    assert!(!result.inputs.contains(&canonical.join("agent/src/main.rs")));
    result.value
}

fn replace(root: &Path, path: &str, from: &str, to: &str) {
    let file = root.join(path);
    let text = std::fs::read_to_string(&file).unwrap();
    assert!(text.contains(from));
    std::fs::write(file, text.replace(from, to)).unwrap();
}

#[test]
fn unrelated_sources_product_versions_docs_and_dev_dependencies_preserve_identity() {
    let root = fixture();
    let before = identity(root.path());
    write(
        root.path(),
        "agent/src/main.rs",
        "fn main() { changed(); }\n",
    );
    write(
        root.path(),
        "host/README.md",
        "Updated host documentation\n",
    );
    write(
        root.path(),
        "host/tests/all.rs",
        "#[test] fn new_test() {}\n",
    );
    replace(root.path(), "Cargo.toml", "0.2.8", "0.2.9");
    replace(root.path(), "Cargo.lock", "0.2.8", "0.2.9");
    replace(
        root.path(),
        "Cargo.lock",
        "checksum = \"cccc\"",
        "checksum = \"eeee\"",
    );
    replace(
        root.path(),
        "Cargo.lock",
        "checksum = \"dddd\"",
        "checksum = \"ffff\"",
    );
    replace(
        root.path(),
        "host/Cargo.toml",
        "test-only = '1'",
        "test-only = '1.0'",
    );
    assert_eq!(identity(root.path()), before);
}

#[test]
fn host_dependency_source_features_build_and_reachable_lock_changes_alter_identity() {
    for (path, before, after) in [
        (
            "host/src/main.rs",
            "fn main() {}",
            "fn main() { changed(); }",
        ),
        ("protocol/src/lib.rs", "u16 = 1", "u16 = 2"),
        (
            "host/Cargo.toml",
            "['runtime']",
            "['runtime', 'new-feature']",
        ),
        ("host/build.rs", "fn main() {}", "fn main() { changed(); }"),
        ("Cargo.lock", "checksum = \"bbbb\"", "checksum = \"eeee\""),
        ("Cargo.toml", "lto = true", "lto = false"),
    ] {
        let root = fixture();
        let previous = identity(root.path());
        replace(root.path(), path, before, after);
        assert_ne!(identity(root.path()), previous, "{path}");
    }
}

#[test]
fn new_sources_and_compiler_target_settings_alter_identity() {
    let root = fixture();
    let previous = identity(root.path());
    write(root.path(), "protocol/src/added.rs", "pub fn added() {}\n");
    assert_ne!(identity(root.path()), previous);
    let settings = BTreeMap::from([("TARGET".into(), "another-target".into())]);
    let changed =
        fingerprint_support::fingerprint(root.path(), &root.path().join("host"), &settings)
            .unwrap();
    assert_ne!(changed.value, identity(root.path()));
    let _settings_reader = fingerprint_support::build_settings;
}

#[test]
fn missing_or_ambiguous_inputs_fail_instead_of_silently_reusing_identity() {
    let root = fixture();
    replace(
        root.path(),
        "Cargo.lock",
        "name = \"transitive\"",
        "name = \"missing\"",
    );
    assert!(
        fingerprint_support::fingerprint(root.path(), &root.path().join("host"), &BTreeMap::new())
            .is_err()
    );
}

#[test]
fn shared_dependency_feature_changes_in_other_workspace_packages_are_tracked() {
    let root = fixture();
    replace(
        root.path(),
        "Cargo.toml",
        "[workspace]\n",
        "[workspace]\nmembers = ['host', 'protocol', 'agent']\n",
    );
    write(
        root.path(),
        "agent/Cargo.toml",
        "[package]\nname = 'agent'\nversion = '0.2.8'\n[dependencies]\nexternal = { version = '1', features = ['runtime'] }\nunrelated = { version = '1', features = ['unused'] }\n",
    );
    let before = identity(root.path());
    replace(
        root.path(),
        "agent/Cargo.toml",
        "['unused']",
        "['another-unused']",
    );
    assert_eq!(identity(root.path()), before);
    replace(
        root.path(),
        "agent/Cargo.toml",
        "['runtime']",
        "['runtime', 'enabled']",
    );
    assert_ne!(identity(root.path()), before);
}

#[test]
fn every_resolved_version_of_an_aliased_production_dependency_is_tracked() {
    let root = fixture();
    replace(
        root.path(),
        "host/Cargo.toml",
        "[dependencies]\n",
        "[dependencies]\nother = { package = 'external', version = '2' }\n",
    );
    replace(
        root.path(),
        "Cargo.lock",
        "\"protocol\", \"external\", \"test-only\"",
        "\"protocol\", \"external 1.0.0\", \"external 2.0.0\", \"test-only\"",
    );
    let lock_path = root.path().join("Cargo.lock");
    let mut lock = std::fs::read_to_string(&lock_path).unwrap();
    lock.push_str("\n[[package]]\nname = 'external'\nversion = '2.0.0'\nsource = 'registry+https://example.test/index'\nchecksum = 'second-version'\n");
    std::fs::write(lock_path, lock).unwrap();
    let before = identity(root.path());
    replace(
        root.path(),
        "Cargo.lock",
        "second-version",
        "updated-second-version",
    );
    assert_ne!(identity(root.path()), before);
}

#[test]
fn shared_dependency_features_from_distinct_targets_do_not_overwrite_each_other() {
    let root = fixture();
    replace(
        root.path(),
        "Cargo.toml",
        "[workspace]\n",
        "[workspace]\nmembers = ['agent']\n",
    );
    write(
        root.path(),
        "agent/Cargo.toml",
        "[package]\nname = 'agent'\nversion = '0.2.8'\n[dependencies]\nexternal = { version = '1', features = ['runtime'] }\n[target.'cfg(windows)'.dependencies]\nexternal = { version = '1', features = ['windows'] }\n",
    );
    let before = identity(root.path());
    replace(
        root.path(),
        "agent/Cargo.toml",
        "['runtime']",
        "['runtime', 'enabled']",
    );
    assert_ne!(identity(root.path()), before);
}
