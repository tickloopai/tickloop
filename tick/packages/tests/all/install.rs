use std::{fs, os::unix::fs::PermissionsExt, path::Path, process::Command, thread, time::Duration};

use serde_json::json;
use tick_packages::{InstallOutcome, PackageError, PackageStore};
use time::{OffsetDateTime, format_description::well_known::Rfc3339};

use crate::support::{Fixture, MANIFEST, link, manifest};

#[test]
fn a_package_directory_installs_with_its_files_and_registration() {
    let fixture = Fixture::new();
    let source = fixture.package("one", "1.0.0");

    let installation = fixture.store.install_path(&source).unwrap();

    assert_eq!(installation.outcome, InstallOutcome::Installed);
    let package = &installation.package;
    let root = fixture
        .packages_directory()
        .canonicalize()
        .unwrap()
        .join("sample");
    assert_eq!(package.root, root);
    assert_eq!(package.manifest.name, "sample");
    assert_eq!(package.manifest.version, "1.0.0");
    assert_eq!(package.executable(), root.join("bin/tick-sample"));
    assert_eq!(package.source, source.to_string_lossy());
    assert_eq!(package.sha256, None);
    OffsetDateTime::parse(&package.installed_at, &Rfc3339).unwrap();
    assert_eq!(
        fs::read(root.join("bin/tick-sample")).unwrap(),
        b"#!/bin/sh\necho one\n"
    );
    let program_mode = fs::metadata(root.join("bin/tick-sample"))
        .unwrap()
        .permissions()
        .mode();
    assert_ne!(program_mode & 0o111, 0, "{program_mode:o}");
    assert!(root.join("skills/sample/SKILL.md").is_file());

    let registry = fixture.registry();
    assert_eq!(
        registry,
        json!({
            "version": 1,
            "packages": { "sample": {
                "version": "1.0.0",
                "source": source.to_string_lossy(),
                "installed_at": package.installed_at,
            }},
        })
    );
    let private_mode = |path: &Path| fs::metadata(path).unwrap().permissions().mode() & 0o777;
    assert_eq!(
        private_mode(&fixture.packages_directory().join("installed.json")),
        0o600
    );
    assert_eq!(private_mode(fixture.packages_directory()), 0o700);
    let installed = fixture.store.installed().unwrap();
    assert_eq!(installed.packages, vec![package.clone()]);
    assert!(installed.broken.is_empty());
    fixture.assert_no_leftovers();
}

#[test]
fn a_package_directory_at_the_installed_version_is_reinstalled() {
    let fixture = Fixture::new();
    let first = fixture
        .store
        .install_path(&fixture.package("one", "1.0.0"))
        .unwrap();
    fs::write(first.package.root.join("marker"), "replaced").unwrap();
    let rebuilt = fixture.package("rebuilt", "1.0.0");

    let again = fixture.store.install_path(&rebuilt).unwrap();

    assert_eq!(again.outcome, InstallOutcome::Reinstalled);
    assert_eq!(again.package.root, first.package.root);
    assert_eq!(again.package.source, rebuilt.to_string_lossy());
    assert!(!again.package.root.join("marker").exists());
    assert_eq!(
        fs::read(again.package.executable()).unwrap(),
        b"#!/bin/sh\necho rebuilt\n"
    );
    assert_eq!(
        fixture.registry()["packages"]["sample"]["source"],
        rebuilt.to_string_lossy().as_ref()
    );
    fixture.assert_no_leftovers();
}

#[test]
fn another_version_replaces_the_installed_one() {
    let fixture = Fixture::new();
    let first = fixture
        .store
        .install_path(&fixture.package("one", "1.0.0"))
        .unwrap();
    fs::write(first.package.root.join("marker"), "replaced").unwrap();
    let source = fixture.package("two", "2.0.0");

    let second = fixture.store.install_path(&source).unwrap();

    assert_eq!(
        second.outcome,
        InstallOutcome::Replaced {
            previous_version: "1.0.0".into()
        }
    );
    assert_eq!(second.package.manifest.version, "2.0.0");
    assert_eq!(second.package.root, first.package.root);
    assert!(!second.package.root.join("marker").exists());
    assert_eq!(
        fs::read(second.package.executable()).unwrap(),
        b"#!/bin/sh\necho two\n"
    );
    let registry = fixture.registry();
    assert_eq!(registry["packages"]["sample"]["version"], "2.0.0");
    assert_eq!(
        registry["packages"]["sample"]["source"],
        source.to_string_lossy().as_ref()
    );
    assert_eq!(
        fixture.store.installed().unwrap().packages,
        vec![second.package]
    );
    fixture.assert_no_leftovers();
}

#[test]
fn a_broken_installation_is_listed_apart_and_installing_again_repairs_it() {
    let fixture = Fixture::new();
    let first = fixture
        .store
        .install_path(&fixture.package("one", "1.0.0"))
        .unwrap();
    fs::remove_file(first.package.executable()).unwrap();

    let installed = fixture.store.installed().unwrap();
    assert!(installed.packages.is_empty());
    assert_eq!(installed.broken.len(), 1);
    assert_eq!(installed.broken[0].name, "sample");
    assert_eq!(installed.broken[0].version, "1.0.0");
    assert!(
        installed.broken[0].reason.contains("does not exist"),
        "{}",
        installed.broken[0].reason
    );

    let repaired = fixture
        .store
        .install_path(&fixture.package("again", "1.0.0"))
        .unwrap();
    assert_eq!(repaired.outcome, InstallOutcome::Reinstalled);
    assert!(repaired.package.executable().is_file());
    assert!(fixture.store.installed().unwrap().broken.is_empty());
}

#[test]
fn uninstalling_removes_the_package_and_its_registration_and_names_the_kept_credentials() {
    let fixture = Fixture::new();
    let mut with_credentials = manifest("sample", "1.0.0");
    with_credentials["credentials"] = json!("~/.tickloop/sample/");
    let installed = fixture
        .store
        .install_path(&fixture.package_with("one", &with_credentials))
        .unwrap();
    fixture
        .store
        .install_path(&fixture.package_with("other", &manifest("other", "3.0.0")))
        .unwrap();

    let uninstalled = fixture.store.uninstall("sample").unwrap();

    assert_eq!(uninstalled.name, "sample");
    assert_eq!(uninstalled.version, "1.0.0");
    assert_eq!(
        uninstalled.credentials.as_deref(),
        Some("~/.tickloop/sample/")
    );
    assert!(!installed.package.root.exists());
    assert_eq!(
        fixture.registry()["packages"]
            .as_object()
            .unwrap()
            .keys()
            .collect::<Vec<_>>(),
        ["other"]
    );
    let names = fixture
        .store
        .installed()
        .unwrap()
        .packages
        .into_iter()
        .map(|package| package.manifest.name)
        .collect::<Vec<_>>();
    assert_eq!(names, ["other"]);
    assert!(matches!(
        fixture.store.uninstall("sample"),
        Err(PackageError::NotInstalled(name)) if name == "sample"
    ));
    assert!(matches!(
        fixture.store.uninstall("../config"),
        Err(PackageError::Invalid(_))
    ));
    fixture.assert_no_leftovers();
}

#[test]
fn uninstalling_from_an_empty_config_root_creates_nothing() {
    let fixture = Fixture::new();
    assert!(matches!(
        fixture.store.uninstall("sample"),
        Err(PackageError::NotInstalled(_))
    ));
    assert!(!fixture.packages_directory().exists());
    let installed = fixture.store.installed().unwrap();
    assert!(installed.packages.is_empty() && installed.broken.is_empty());
}

#[test]
fn invalid_packages_fail_and_leave_the_installed_package_unchanged() {
    let fixture = Fixture::new();
    fixture
        .store
        .install_path(&fixture.package("one", "1.0.0"))
        .unwrap();
    let before = fixture.snapshot();
    let outside = fixture.root().join("outside");
    fs::write(&outside, "#!/bin/sh\n").unwrap();
    fs::set_permissions(&outside, fs::Permissions::from_mode(0o755)).unwrap();

    type Breakage = Box<dyn Fn(&Path)>;
    let with_manifest = |change: fn(&mut serde_json::Value)| -> Breakage {
        Box::new(move |package: &Path| {
            let mut manifest = manifest("sample", "2.0.0");
            change(&mut manifest);
            fs::write(package.join(MANIFEST), manifest.to_string()).unwrap();
        })
    };
    let outside_for_link = outside.clone();
    let cases: Vec<(&str, Breakage, &str)> = vec![
        (
            "missing field",
            with_manifest(|manifest| {
                manifest.as_object_mut().unwrap().remove("executable");
            }),
            "missing field `executable`",
        ),
        (
            "no manifest",
            Box::new(|package| fs::remove_file(package.join(MANIFEST)).unwrap()),
            "has no tick-package.json",
        ),
        (
            "invalid name",
            with_manifest(|manifest| manifest["name"] = json!("Sample")),
            "is not a Tick package name",
        ),
        (
            "missing executable",
            Box::new(|package| fs::remove_file(package.join("bin/tick-sample")).unwrap()),
            "does not exist",
        ),
        (
            "executable without execute permission",
            Box::new(|package| {
                fs::set_permissions(
                    package.join("bin/tick-sample"),
                    fs::Permissions::from_mode(0o644),
                )
                .unwrap()
            }),
            "is not executable",
        ),
        (
            "executable through ..",
            with_manifest(|manifest| manifest["executable"] = json!("bin/../../tick-sample")),
            "must be a relative path inside the package",
        ),
        (
            "absolute executable",
            with_manifest(|manifest| manifest["executable"] = json!("/bin/tick-sample")),
            "must be a relative path inside the package",
        ),
        (
            "executable of another name",
            with_manifest(|manifest| manifest["executable"] = json!("bin/tick-other")),
            "must be named tick-sample",
        ),
        (
            "executable linked outside",
            Box::new(move |package| {
                fs::remove_file(package.join("bin/tick-sample")).unwrap();
                link(&outside_for_link, package.join("bin/tick-sample"));
            }),
            "symbolic link bin/tick-sample must lead to a file or directory inside the package",
        ),
        (
            "link outside the package",
            Box::new(|package| link("../../..", package.join("skills/escape"))),
            "symbolic link skills/escape must lead",
        ),
        (
            "dangling link",
            Box::new(|package| link("missing", package.join("skills/missing"))),
            "symbolic link skills/missing must lead",
        ),
        (
            "named pipe",
            Box::new(|package| {
                let status = Command::new("mkfifo")
                    .arg(package.join("skills/pipe"))
                    .status()
                    .unwrap();
                assert!(status.success());
            }),
            "skills/pipe is neither a regular file, a directory nor a symbolic link",
        ),
    ];

    for (label, breakage, expected) in cases {
        let source = fixture.package(&label.replace(' ', "-"), "2.0.0");
        breakage(&source);
        let error = fixture.store.install_path(&source).unwrap_err().to_string();
        assert!(error.contains(expected), "{label}: {error}");
        assert_eq!(fixture.snapshot(), before, "{label}");
        fixture.assert_no_leftovers();
    }
}

#[test]
fn a_package_is_found_by_name() {
    let fixture = Fixture::new();
    let installed = fixture
        .store
        .install_path(&fixture.package("one", "1.0.0"))
        .unwrap()
        .package;

    assert_eq!(fixture.store.get("sample").unwrap(), installed);
    assert!(matches!(
        fixture.store.get("other"),
        Err(PackageError::NotInstalled(name)) if name == "other"
    ));
    assert!(matches!(
        fixture.store.get("../sample"),
        Err(PackageError::Invalid(_))
    ));
    fs::remove_file(installed.executable()).unwrap();
    let error = fixture.store.get("sample").unwrap_err();
    assert!(
        matches!(&error, PackageError::Unusable { name, reason }
            if name == "sample" && reason.contains("does not exist")),
        "{error}"
    );
}

#[test]
fn a_reader_waits_for_an_install_under_way_instead_of_seeing_a_half_replaced_package() {
    let fixture = Fixture::new();
    let installed = fixture
        .store
        .install_path(&fixture.package("one", "1.0.0"))
        .unwrap()
        .package;
    let writer = fs::File::open(fixture.packages_directory().join(".lock")).unwrap();
    writer.lock().unwrap();
    let aside = fixture.root().join("aside");
    fs::rename(&installed.root, &aside).unwrap();

    let store = fixture.store.clone();
    let reader = thread::spawn(move || (store.get("sample"), store.installed()));
    thread::sleep(Duration::from_millis(300));
    assert!(
        !reader.is_finished(),
        "the reader did not wait for the writer"
    );
    fs::rename(&aside, &installed.root).unwrap();
    writer.unlock().unwrap();

    let (found, listed) = reader.join().unwrap();
    assert_eq!(found.unwrap(), installed);
    let listed = listed.unwrap();
    assert_eq!(listed.packages, vec![installed]);
    assert!(listed.broken.is_empty());
}

#[cfg(target_os = "macos")]
#[test]
fn a_registry_that_cannot_be_written_puts_the_installed_package_back() {
    /// Marks a file immutable, so renaming the new registry over it fails, until dropped.
    struct Immutable(std::path::PathBuf);

    impl Immutable {
        fn new(path: std::path::PathBuf) -> Self {
            let status = Command::new("chflags").arg("uchg").arg(&path).status();
            assert!(status.unwrap().success());
            Self(path)
        }
    }

    impl Drop for Immutable {
        fn drop(&mut self) {
            let _ = Command::new("chflags").arg("nouchg").arg(&self.0).status();
        }
    }

    let fixture = Fixture::new();
    fixture
        .store
        .install_path(&fixture.package("one", "1.0.0"))
        .unwrap();
    let registry = fixture.packages_directory().join("installed.json");
    let _immutable = Immutable::new(registry.clone());
    let before = fixture.snapshot();

    let install = fixture
        .store
        .install_path(&fixture.package("two", "2.0.0"))
        .unwrap_err()
        .to_string();
    assert!(
        install.starts_with(&format!("cannot write {}", registry.display())),
        "{install}"
    );
    assert_eq!(fixture.snapshot(), before);
    fixture.assert_no_leftovers();

    let uninstall = fixture.store.uninstall("sample").unwrap_err().to_string();
    assert!(
        uninstall.starts_with(&format!("cannot write {}", registry.display())),
        "{uninstall}"
    );
    assert_eq!(fixture.snapshot(), before);
    fixture.assert_no_leftovers();
    assert_eq!(
        fixture.store.get("sample").unwrap().manifest.version,
        "1.0.0"
    );
}

#[test]
fn a_link_inside_the_package_is_kept_as_a_link() {
    let fixture = Fixture::new();
    let source = fixture.package("one", "1.0.0");
    link("sample", source.join("skills/alias"));

    let package = fixture.store.install_path(&source).unwrap().package;

    let alias = package.root.join("skills/alias");
    assert!(
        fs::symlink_metadata(&alias)
            .unwrap()
            .file_type()
            .is_symlink()
    );
    assert!(alias.join("SKILL.md").is_file());
}

#[test]
fn a_damaged_registry_is_reported_and_left_alone() {
    let fixture = Fixture::new();
    fixture
        .store
        .install_path(&fixture.package("one", "1.0.0"))
        .unwrap();
    let registry = fixture.packages_directory().join("installed.json");
    for damaged in [
        "{\"version\": 1, \"packages\": ",
        "{\"version\": 2, \"packages\": {}}",
    ] {
        fs::write(&registry, damaged).unwrap();
        let before = fixture.snapshot();

        let listing = fixture.store.installed().unwrap_err();
        let install = fixture
            .store
            .install_path(&fixture.package("two", "2.0.0"))
            .unwrap_err();
        let uninstall = fixture.store.uninstall("sample").unwrap_err();

        for error in [listing, install, uninstall] {
            assert!(matches!(error, PackageError::Registry { .. }), "{error}");
            let message = error.to_string();
            assert!(
                message.contains(&registry.display().to_string()),
                "{message}"
            );
            assert!(message.contains("is damaged"), "{message}");
        }
        assert_eq!(fixture.snapshot(), before);
        fixture.assert_no_leftovers();
    }
}

#[test]
fn a_directory_holding_the_packages_directory_is_refused() {
    let fixture = Fixture::new();
    let root = fixture.root();
    fs::write(root.join(MANIFEST), manifest("sample", "1.0.0").to_string()).unwrap();

    let error = fixture.store.install_path(&root).unwrap_err().to_string();

    assert!(
        error.contains("contains the Tick packages directory"),
        "{error}"
    );
    assert!(fixture.store.installed().unwrap().packages.is_empty());
    fixture.assert_no_leftovers();
}

#[test]
fn a_store_finds_its_packages_under_the_config_root() {
    let store = PackageStore::in_config_root(Path::new("/config"));
    assert_eq!(store.directory(), Path::new("/config/ticks"));
}
