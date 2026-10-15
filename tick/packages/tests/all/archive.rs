use std::{fs, os::unix::fs::PermissionsExt, path::Path};

use flate2::{Compression, write::GzEncoder};
use sha2::{Digest, Sha256};
use tar::EntryType;
use tick_packages::{ExpectedArchive, InstallOutcome};

use crate::support::{
    Fixture, MANIFEST, RawEntry, archive_of, link, manifest, raw_archive, sample_entries,
};

const DOWNLOAD: &str = "https://example.com/tick-sample-1.0.0-aarch64-apple-darwin.tar.gz";

fn sha256_of(archive: &Path) -> String {
    format!("{:x}", Sha256::digest(fs::read(archive).unwrap()))
}

fn expected<'a>(sha256: &'a str) -> ExpectedArchive<'a> {
    ExpectedArchive {
        name: "sample",
        version: "1.0.0",
        sha256,
        source: DOWNLOAD,
    }
}

#[test]
fn an_archive_installs_and_records_its_source_and_sha256() {
    let fixture = Fixture::new();
    let source = fixture.package("one", "1.0.0");
    link("sample", source.join("skills/alias"));
    let archive = fixture.root().join("sample.tar.gz");
    archive_of(&source, &archive);
    let sha256 = sha256_of(&archive);

    let installation = fixture
        .store
        .install_archive(&archive, expected(&sha256.to_uppercase()))
        .unwrap();

    assert_eq!(installation.outcome, InstallOutcome::Installed);
    let package = installation.package;
    assert_eq!(package.source, DOWNLOAD);
    assert_eq!(package.sha256.as_deref(), Some(sha256.as_str()));
    assert_eq!(fixture.registry()["packages"]["sample"]["sha256"], sha256);
    let mode = fs::metadata(package.executable())
        .unwrap()
        .permissions()
        .mode();
    assert_ne!(mode & 0o111, 0, "{mode:o}");
    assert!(package.root.join("skills/sample/SKILL.md").is_file());
    assert!(
        fs::symlink_metadata(package.root.join("skills/alias"))
            .unwrap()
            .file_type()
            .is_symlink()
    );
    assert_eq!(fixture.store.installed().unwrap().packages, vec![package]);
    fixture.assert_no_leftovers();
}

#[test]
fn an_expected_archive_at_the_installed_version_changes_nothing() {
    let fixture = Fixture::new();
    let archive = fixture.root().join("sample.tar.gz");
    archive_of(&fixture.package("one", "1.0.0"), &archive);
    let sha256 = sha256_of(&archive);
    let first = fixture
        .store
        .install_archive(&archive, expected(&sha256))
        .unwrap();
    fs::write(first.package.root.join("marker"), "kept").unwrap();
    let before = fixture.snapshot();

    let again = fixture
        .store
        .install_archive(&archive, expected(&sha256))
        .unwrap();

    assert_eq!(again.outcome, InstallOutcome::AlreadyInstalled);
    assert_eq!(again.package, first.package);
    assert_eq!(fixture.snapshot(), before);
    fixture.assert_no_leftovers();
}

#[test]
fn a_local_archive_path_installs_with_its_path_as_the_source_and_always_replaces() {
    let fixture = Fixture::new();
    let archive = fixture.root().join("sample.tar.gz");
    archive_of(&fixture.package("one", "1.0.0"), &archive);

    let first = fixture.store.install_path(&archive).unwrap();
    fs::write(first.package.root.join("marker"), "replaced").unwrap();
    let again = fixture.store.install_path(&archive).unwrap();

    assert_eq!(first.outcome, InstallOutcome::Installed);
    assert_eq!(again.outcome, InstallOutcome::Reinstalled);
    assert_eq!(again.package.source, archive.to_string_lossy());
    assert_eq!(
        again.package.sha256.as_deref(),
        Some(sha256_of(&archive).as_str())
    );
    assert!(!again.package.root.join("marker").exists());
    fixture.assert_no_leftovers();
}

#[test]
fn an_archive_that_is_not_what_was_expected_is_refused() {
    let fixture = Fixture::new();
    fixture
        .store
        .install_path(&fixture.package("installed", "0.9.0"))
        .unwrap();
    let before = fixture.snapshot();
    let archive = fixture.root().join("sample.tar.gz");
    archive_of(&fixture.package("one", "1.0.0"), &archive);
    let sha256 = sha256_of(&archive);
    let other_sha256 = "0".repeat(64);

    let cases = [
        (
            ExpectedArchive {
                sha256: &other_sha256,
                ..expected(&sha256)
            },
            format!("the archive's SHA-256 is {sha256}, not {other_sha256}"),
        ),
        (
            ExpectedArchive {
                name: "longbridge",
                ..expected(&sha256)
            },
            "the package is named sample, not longbridge".to_owned(),
        ),
        (
            ExpectedArchive {
                version: "2.0.0",
                ..expected(&sha256)
            },
            "the package is version 1.0.0, not 2.0.0".to_owned(),
        ),
    ];

    for (expectation, message) in cases {
        let error = fixture
            .store
            .install_archive(&archive, expectation)
            .unwrap_err()
            .to_string();
        assert_eq!(error, message);
        assert_eq!(fixture.snapshot(), before);
        fixture.assert_no_leftovers();
    }
}

#[test]
fn archives_that_reach_outside_the_package_or_hold_other_files_are_refused() {
    let fixture = Fixture::new();
    fixture
        .store
        .install_path(&fixture.package("one", "1.0.0"))
        .unwrap();
    let before = fixture.snapshot();
    let outside = fixture.root().join("outside");
    fs::create_dir(&outside).unwrap();
    let manifest_json = manifest("sample", "2.0.0").to_string();

    let cases: Vec<(&str, Vec<RawEntry>, &str)> = vec![
        (
            "parent directory",
            vec![RawEntry::file("../escaped", b"x", 0o644)],
            "archive entry ../escaped lies outside the package",
        ),
        (
            "absolute path",
            vec![RawEntry::file("/tickloop-escaped", b"x", 0o644)],
            "archive entry /tickloop-escaped lies outside the package",
        ),
        (
            "link where files of the archive already made a directory",
            vec![
                RawEntry::file(MANIFEST, manifest_json.as_bytes(), 0o644),
                RawEntry::link("bin", EntryType::Symlink, "../../../../outside"),
                RawEntry::file("bin/tick-sample", b"#!/bin/sh\n", 0o755),
            ],
            "cannot unpack bin",
        ),
        (
            "link that leads outside",
            [
                sample_entries(manifest_json.as_bytes()),
                vec![RawEntry::link(
                    "skills/escape",
                    EntryType::Symlink,
                    "../../..",
                )],
            ]
            .concat(),
            "symbolic link skills/escape must lead",
        ),
        (
            "link written through another link",
            [
                sample_entries(manifest_json.as_bytes()),
                vec![
                    RawEntry::link("skills/up", EntryType::Symlink, ".."),
                    RawEntry::link("skills/up/escape", EntryType::Symlink, "../.."),
                ],
            ]
            .concat(),
            "skills/up in the package is not a directory",
        ),
        (
            "hard link",
            [
                sample_entries(manifest_json.as_bytes()),
                vec![RawEntry::link(
                    "bin/copy",
                    EntryType::Link,
                    "bin/tick-sample",
                )],
            ]
            .concat(),
            "bin/copy is neither a regular file, a directory nor a symbolic link",
        ),
        (
            "device with control characters in its name",
            [
                sample_entries(manifest_json.as_bytes()),
                vec![RawEntry {
                    path: "skills/\u{1b}[31mdevice",
                    entry_type: EntryType::Char,
                    link: None,
                    data: b"",
                    mode: 0o644,
                }],
            ]
            .concat(),
            "skills/\\u{1b}[31mdevice is neither a regular file",
        ),
    ];

    for (label, entries, expected) in cases {
        let archive = fixture
            .root()
            .join(format!("{}.tar.gz", label.replace(' ', "-")));
        raw_archive(&archive, &entries);
        let error = fixture
            .store
            .install_path(&archive)
            .unwrap_err()
            .to_string();
        assert!(error.contains(expected), "{label}: {error}");
        assert_eq!(fixture.snapshot(), before, "{label}");
        assert_eq!(fs::read_dir(&outside).unwrap().count(), 0, "{label}");
        assert!(!fixture.root().join("escaped").exists(), "{label}");
        fixture.assert_no_leftovers();
    }
}

#[test]
fn an_archive_that_unpacks_beyond_the_limit_is_refused_before_writing_it() {
    let fixture = Fixture::new();
    let archive = fixture.root().join("oversized.tar.gz");
    let mut builder = tar::Builder::new(GzEncoder::new(
        fs::File::create(&archive).unwrap(),
        Compression::default(),
    ));
    let mut header = tar::Header::new_gnu();
    header.set_path("bin/tick-sample").unwrap();
    header.set_entry_type(EntryType::Regular);
    header.set_size(513 * 1024 * 1024);
    header.set_mode(0o755);
    header.set_cksum();
    builder.append(&header, &[][..]).unwrap();
    builder.into_inner().unwrap().finish().unwrap();

    let error = fixture
        .store
        .install_path(&archive)
        .unwrap_err()
        .to_string();

    assert_eq!(error, "the archive unpacks to more than 512 MiB");
    assert!(fixture.store.installed().unwrap().packages.is_empty());
    fixture.assert_no_leftovers();
}

#[test]
fn a_damaged_archive_is_refused() {
    let fixture = Fixture::new();
    let valid = fixture.root().join("valid.tar.gz");
    archive_of(&fixture.package("one", "1.0.0"), &valid);
    let bytes = fs::read(&valid).unwrap();
    let not_gzip = fixture.root().join("not-gzip.tar.gz");
    fs::write(&not_gzip, b"not an archive").unwrap();
    let truncated = fixture.root().join("truncated.tar.gz");
    fs::write(&truncated, &bytes[..bytes.len() / 2]).unwrap();

    for archive in [not_gzip, truncated] {
        let error = fixture
            .store
            .install_path(&archive)
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("not a readable .tar.gz") || error.contains("cannot unpack"),
            "{error}"
        );
        assert!(fixture.store.installed().unwrap().packages.is_empty());
        fixture.assert_no_leftovers();
    }
}
