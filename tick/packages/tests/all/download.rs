use std::{fs, io::Write};

use sha2::{Digest, Sha256};
use tick_packages::{ExpectedArchive, InstallOutcome};

use crate::support::{Fixture, archive_of};

#[test]
fn a_download_file_installs_and_is_removed_when_dropped() {
    let fixture = Fixture::new();
    let archive = fixture.root().join("sample.tar.gz");
    archive_of(&fixture.package("one", "1.0.0"), &archive);
    let bytes = fs::read(&archive).unwrap();
    let sha256 = format!("{:x}", Sha256::digest(&bytes));

    let mut download = fixture.store.download_file().unwrap();
    download.write_all(&bytes).unwrap();
    assert!(download.path().starts_with(fixture.packages_directory()));
    let installation = fixture
        .store
        .install_archive(
            download.path(),
            ExpectedArchive {
                name: "sample",
                version: "1.0.0",
                sha256: &sha256,
                source: "https://example.com/sample.tar.gz",
            },
        )
        .unwrap();
    let path = download.path().to_path_buf();
    drop(download);

    assert_eq!(installation.outcome, InstallOutcome::Installed);
    assert!(!path.exists());
    fixture.assert_no_leftovers();
}

#[test]
fn only_a_download_file_no_process_holds_is_removed_by_the_next_install() {
    let fixture = Fixture::new();
    let in_progress = fixture.store.download_file().unwrap();
    let abandoned = fixture.packages_directory().join(".download-abandoned");
    fs::write(&abandoned, "partial").unwrap();

    fixture
        .store
        .install_path(&fixture.package("one", "1.0.0"))
        .unwrap();

    assert!(!abandoned.exists());
    assert!(in_progress.path().is_file());
    fs::write(&abandoned, "partial").unwrap();
    fixture.store.uninstall("sample").unwrap();
    assert!(!abandoned.exists());
    assert!(in_progress.path().is_file());
    drop(in_progress);
    fixture.assert_no_leftovers();
}
