use std::io::Write;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::Path;

use flate2::{Compression, write::GzEncoder};
use sha2::{Digest, Sha256};

use crate::{
    Build, Installation, Lock, PROGRAMS, Source, UpdateState, bundle, is_due, release, source,
};

fn build(version: &str) -> Build {
    Build {
        version: version.into(),
        identity: format!("{version}+1234567890abcdef"),
    }
}

fn programs(directory: &Path, build: &Build) {
    std::fs::create_dir_all(directory).unwrap();
    for name in PROGRAMS {
        std::fs::write(directory.join(name), format!("#!/bin/sh\ncase \"$1\" in\n--version) echo '{name} {}';;\n--build-identity) echo '{}';;\n*) exit 2;;\nesac\n", build.version, build.identity)).unwrap();
        std::fs::set_permissions(directory.join(name), std::fs::Permissions::from_mode(0o700))
            .unwrap();
    }
}

fn archive(entries: &[(&str, tar::EntryType)]) -> Vec<u8> {
    let mut archive = tar::Builder::new(GzEncoder::new(Vec::new(), Compression::default()));
    for (name, kind) in entries {
        let mut header = tar::Header::new_gnu();
        header.set_size(1);
        header.set_entry_type(*kind);
        header.set_mode(0o755);
        // Raw names exercise malicious paths that tar's own safe builder refuses to produce.
        header.as_mut_bytes()[..name.len()].copy_from_slice(name.as_bytes());
        if kind.is_symlink() {
            header.set_link_name("/tmp/escape").unwrap();
        }
        header.set_cksum();
        archive.append(&header, &b"x"[..]).unwrap();
    }
    archive.into_inner().unwrap().finish().unwrap()
}

#[test]
fn archive_rejects_bad_checksum_without_writing() {
    let root = tempfile::tempdir().unwrap();
    let bytes = archive(&[("tickloop", tar::EntryType::Regular)]);
    assert!(bundle::unpack(&bytes, &"0".repeat(64), root.path()).is_err());
    assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 0);
}

#[test]
fn archive_rejects_traversal_links_duplicates_and_missing_programs() {
    for entries in [
        vec![("../escape", tar::EntryType::Regular)],
        vec![("/escape", tar::EntryType::Regular)],
        vec![("tickloop", tar::EntryType::Symlink)],
        vec![("tickloop", tar::EntryType::Link)],
        vec![
            ("tickloop", tar::EntryType::Regular),
            ("tickloop", tar::EntryType::Regular),
        ],
        vec![("tickloop", tar::EntryType::Regular)],
    ] {
        let root = tempfile::tempdir().unwrap();
        let bytes = archive(&entries);
        let hash = format!("{:x}", Sha256::digest(&bytes));
        assert!(
            bundle::unpack(&bytes, &hash, root.path()).is_err(),
            "{entries:?}"
        );
    }
}

#[test]
fn archive_unpacks_only_complete_regular_bundle_with_private_execute_permissions() {
    let root = tempfile::tempdir().unwrap();
    let entries = PROGRAMS.map(|name| (name, tar::EntryType::Regular));
    let bytes = archive(&entries);
    bundle::unpack(
        &bytes,
        &format!("{:x}", Sha256::digest(&bytes)),
        root.path(),
    )
    .unwrap();
    for name in PROGRAMS {
        assert_eq!(std::fs::read(root.path().join(name)).unwrap(), b"x");
        assert_eq!(
            std::fs::metadata(root.path().join(name))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o500
        );
    }
}

#[test]
fn complete_bundle_switch_preserves_old_programs_and_refuses_same_version_replacement() {
    let root = tempfile::tempdir().unwrap();
    let stage_a = tempfile::tempdir_in(root.path()).unwrap();
    programs(stage_a.path(), &build("1.0.0"));
    bundle::verify(stage_a.path(), &build("1.0.0")).unwrap();
    bundle::activate(root.path(), stage_a.path(), &build("1.0.0")).unwrap();
    let old = root.path().join("current/tickloop").canonicalize().unwrap();
    let stage_b = tempfile::tempdir_in(root.path()).unwrap();
    programs(stage_b.path(), &build("1.1.0"));
    bundle::activate(root.path(), stage_b.path(), &build("1.1.0")).unwrap();
    assert_eq!(
        bundle::read_build(&root.path().join("current"))
            .unwrap()
            .version,
        "1.1.0"
    );
    assert!(
        std::process::Command::new(&old)
            .arg("--version")
            .output()
            .unwrap()
            .status
            .success()
    );
    let malicious = tempfile::tempdir_in(root.path()).unwrap();
    programs(malicious.path(), &build("1.1.0"));
    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .open(malicious.path().join("tickhost"))
        .unwrap();
    writeln!(file, "# modified").unwrap();
    assert!(bundle::activate(root.path(), malicious.path(), &build("1.1.0")).is_err());
    assert_eq!(
        std::fs::read_link(root.path().join("current")).unwrap(),
        Path::new("versions/1.1.0")
    );
}

#[test]
fn mixed_or_linked_programs_fail_before_activation() {
    let root = tempfile::tempdir().unwrap();
    programs(root.path(), &build("1.0.0"));
    std::fs::write(root.path().join("tickhost"), "#!/bin/sh\necho other\n").unwrap();
    assert!(bundle::verify(root.path(), &build("1.0.0")).is_err());
    std::fs::remove_file(root.path().join("tickhost")).unwrap();
    symlink("tickloop", root.path().join("tickhost")).unwrap();
    assert!(bundle::verify(root.path(), &build("1.0.0")).is_err());
}

#[test]
fn file_lock_excludes_concurrent_installers_and_recovers_after_drop() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("install.lock");
    let first = Lock::acquire(&path).unwrap();
    assert!(Lock::acquire(&path).is_err());
    drop(first);
    assert!(Lock::acquire(&path).is_ok());
}

#[test]
fn installer_lock_refuses_a_symlink() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("other"), b"untouched").unwrap();
    symlink("other", root.path().join("install.lock")).unwrap();
    assert!(Lock::acquire(&root.path().join("install.lock")).is_err());
    assert_eq!(
        std::fs::read(root.path().join("other")).unwrap(),
        b"untouched"
    );
}

#[test]
fn metadata_only_accepts_matching_complete_stable_assets() {
    let value = |tag: &str| {
        serde_json::json!({"tag_name": tag, "draft": false, "prerelease": false, "assets": [
            {"name":"tickloop-aarch64-apple-darwin.tar.gz", "browser_download_url":format!("https://github.com/tickloopai/homebrew-tap/releases/download/{tag}/tickloop-aarch64-apple-darwin.tar.gz")},
            {"name":"tickloop-aarch64-apple-darwin.tar.gz.sha256", "browser_download_url":format!("https://github.com/tickloopai/homebrew-tap/releases/download/{tag}/tickloop-aarch64-apple-darwin.tar.gz.sha256")}
        ]})
    };
    let good = value("v1.2.3");
    assert!(
        release::candidate(
            serde_json::from_value(good.clone()).unwrap(),
            "aarch64-apple-darwin"
        )
        .is_ok()
    );
    for mutated in [
        {
            let mut v = good.clone();
            v["assets"][0]["browser_download_url"] = "https://evil.example/a".into();
            v
        },
        {
            let mut v = good.clone();
            v["draft"] = true.into();
            v
        },
        {
            let mut v = good.clone();
            v["assets"].as_array_mut().unwrap().pop();
            v
        },
        value("v1.2.3-beta.1"),
    ] {
        assert!(
            release::candidate(
                serde_json::from_value(mutated).unwrap(),
                "aarch64-apple-darwin"
            )
            .is_err()
        );
    }
}

#[test]
fn checksum_is_bound_to_the_archive_name() {
    let hash = "a".repeat(64);
    assert_eq!(
        release::checksum(
            format!("{hash}  bundle.tar.gz\n").as_bytes(),
            "bundle.tar.gz"
        )
        .unwrap(),
        hash
    );
    assert!(
        release::checksum(
            format!("{hash}  other.tar.gz\n").as_bytes(),
            "bundle.tar.gz"
        )
        .is_err()
    );
    assert!(release::checksum(b"not-a-hash bundle.tar.gz", "bundle.tar.gz").is_err());
}

#[tokio::test]
async fn update_transport_refuses_plaintext_before_network_access() {
    assert!(
        release::get(
            &reqwest::Url::parse("http://127.0.0.1:1/test").unwrap(),
            4096
        )
        .await
        .is_err()
    );
}

#[test]
fn checks_are_throttled_after_success_and_retried_after_failure() {
    let at = 100_000;
    assert!(is_due(&UpdateState::default(), at));
    assert!(!is_due(
        &UpdateState {
            attempted_at: at - 1,
            ..Default::default()
        },
        at
    ));
    assert!(is_due(
        &UpdateState {
            attempted_at: at - 3601,
            ..Default::default()
        },
        at
    ));
    assert!(!is_due(
        &UpdateState {
            checked_at: at - 3601,
            ..Default::default()
        },
        at
    ));
}

fn cargo_metadata(root: &Path, version: &str, revision: &str) {
    let mut installs = serde_json::Map::new();
    for name in [
        "tickloop-agent",
        "tickloop-tickhost",
        "tickloop-tick-reader",
    ] {
        installs.insert(format!("{name} {version} (git+https://github.com/example/tickloop?tag=v{version}#{revision})"), serde_json::json!({}));
    }
    std::fs::write(
        root.join(".crates2.json"),
        serde_json::to_vec(&serde_json::json!({"installs":installs})).unwrap(),
    )
    .unwrap();
}

#[test]
fn cargo_source_preserves_repository_and_pins_complete_revision() {
    let root = tempfile::tempdir().unwrap();
    let revision = "a".repeat(40);
    cargo_metadata(root.path(), "1.2.3", &revision);
    let (repository, version) = source::cargo_source(root.path()).unwrap().unwrap();
    assert_eq!(repository, "https://github.com/example/tickloop");
    assert_eq!(version.to_string(), "1.2.3");
    assert_eq!(source::cargo_revision(root.path()).unwrap(), revision);
    let mut metadata: serde_json::Value =
        serde_json::from_slice(&std::fs::read(root.path().join(".crates2.json")).unwrap()).unwrap();
    metadata["installs"].as_object_mut().unwrap().insert(
        format!("tickloop-agent 1.2.4 (git+https://github.com/example/tickloop#{revision})"),
        serde_json::json!({}),
    );
    std::fs::write(
        root.path().join(".crates2.json"),
        serde_json::to_vec(&metadata).unwrap(),
    )
    .unwrap();
    // Ambiguous installs must not silently choose an arbitrary source/version.
    assert!(source::cargo_source(root.path()).is_err());
}

#[test]
fn native_record_round_trips_without_network_or_requiring_user_home() {
    let root = tempfile::tempdir().unwrap();
    let install = Installation {
        source: Source::Native {
            root: root.path().into(),
        },
        build: build("1.0.0"),
        pinned: false,
    };
    install.write(root.path()).unwrap();
    let found = Installation::detect(&root.path().join("tickloop"))
        .unwrap()
        .unwrap();
    assert!(matches!(found.source, Source::Native { .. }));
}

#[test]
fn pinned_bundle_still_starts_workers_after_original_installation_is_removed() {
    let original = tempfile::tempdir().unwrap();
    let retained = tempfile::tempdir().unwrap();
    let version = build("1.2.3");
    programs(original.path(), &version);
    bundle::copy_programs(original.path(), retained.path()).unwrap();
    bundle::verify(retained.path(), &version).unwrap();
    original.close().unwrap();
    for name in PROGRAMS {
        let output = std::process::Command::new(retained.path().join(name))
            .arg("--version")
            .output()
            .unwrap();
        assert!(output.status.success());
        assert_eq!(
            String::from_utf8(output.stdout).unwrap().trim(),
            format!("{name} 1.2.3")
        );
    }
}

#[test]
fn stale_staging_cleanup_preserves_versions_and_does_not_follow_links() {
    let root = tempfile::tempdir().unwrap();
    let elsewhere = tempfile::tempdir().unwrap();
    std::fs::write(elsewhere.path().join("keep"), b"keep").unwrap();
    std::fs::create_dir(root.path().join(".download-abandoned")).unwrap();
    std::fs::create_dir(root.path().join("versions")).unwrap();
    symlink(elsewhere.path(), root.path().join(".download-linked")).unwrap();
    bundle::remove_staging(root.path(), &[".download-"]).unwrap();
    assert!(!root.path().join(".download-abandoned").exists());
    assert!(root.path().join("versions").exists());
    assert!(elsewhere.path().join("keep").exists());
}

#[test]
fn cargo_rejects_mixed_revisions_even_if_version_and_repository_match() {
    let root = tempfile::tempdir().unwrap();
    cargo_metadata(root.path(), "1.2.3", &"a".repeat(40));
    let text = std::fs::read_to_string(root.path().join(".crates2.json")).unwrap();
    let old = format!(
        "tickloop-agent 1.2.3 (git+https://github.com/example/tickloop?tag=v1.2.3#{})",
        "a".repeat(40)
    );
    let replacement = old.replace(&"a".repeat(40), &"b".repeat(40));
    std::fs::write(
        root.path().join(".crates2.json"),
        text.replace(&old, &replacement),
    )
    .unwrap();
    assert!(source::cargo_source(root.path()).is_err());
    assert!(source::cargo_revision(root.path()).is_err());
}

#[test]
fn brew_source_requires_the_official_tap_receipt() {
    let root = tempfile::tempdir().unwrap();
    let prefix = root.path().join("Cellar/tickloop/1.2.3");
    let bin = prefix.join("bin");
    programs(&bin, &build("1.2.3"));
    std::fs::create_dir(root.path().join("bin")).unwrap();
    std::fs::write(root.path().join("bin/brew"), "brew").unwrap();
    std::fs::write(
        prefix.join("INSTALL_RECEIPT.json"),
        r#"{"source":{"tap":"other/tap"}}"#,
    )
    .unwrap();
    assert!(
        Installation::detect(&bin.join("tickloop"))
            .unwrap()
            .is_none()
    );
    std::fs::write(
        prefix.join("INSTALL_RECEIPT.json"),
        r#"{"source":{"tap":"tickloopai/tap"}}"#,
    )
    .unwrap();
    assert!(matches!(
        Installation::detect(&bin.join("tickloop"))
            .unwrap()
            .unwrap()
            .source,
        Source::Homebrew { .. }
    ));
}

#[test]
fn cargo_path_install_is_retained_as_a_checkout_instead_of_switching_to_binary_releases() {
    let root = tempfile::tempdir().unwrap();
    let cargo = root.path().join("cargo");
    programs(&cargo.join("bin"), &build("1.2.3"));
    let checkout = root.path().join("checkout");
    let mut installs = serde_json::Map::new();
    for (name, path) in [
        ("tickloop-agent", "agent/tickloop"),
        ("tickloop-tickhost", "tickhost/host"),
        ("tickloop-tick-reader", "tick/reader"),
    ] {
        let url = reqwest::Url::from_file_path(checkout.join(path)).unwrap();
        installs.insert(format!("{name} 1.2.3 (path+{url})"), serde_json::json!({}));
    }
    std::fs::write(
        cargo.join(".crates2.json"),
        serde_json::to_vec(&serde_json::json!({"installs":installs})).unwrap(),
    )
    .unwrap();
    let found = Installation::detect(&cargo.join("bin/tickloop"))
        .unwrap()
        .unwrap();
    assert!(matches!(found.source, Source::Checkout { root } if root == checkout));
}

#[tokio::test]
async fn cancelling_an_installer_stops_its_own_descendant_process_group() {
    let root = tempfile::tempdir().unwrap();
    let script = root.path().join("installer.sh");
    let started = root.path().join("started");
    let finished = root.path().join("finished");
    std::fs::write(
        &script,
        "printf started > \"$1\"\n(sleep 1; printf leaked > \"$2\") &\nwait\n",
    )
    .unwrap();
    let task_started = started.clone();
    let task_finished = finished.clone();
    let task = tokio::spawn(async move {
        let mut command = tokio::process::Command::new("/bin/sh");
        command.arg(script).arg(task_started).arg(task_finished);
        crate::run_process(&mut command).await
    });
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while !started.exists() {
        assert!(
            std::time::Instant::now() < deadline,
            "installer never started"
        );
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    tokio::time::sleep(std::time::Duration::from_millis(1300)).await;
    assert!(
        !finished.exists(),
        "a cancelled installer left a descendant running"
    );
}

#[test]
fn publisher_system_tar_format_round_trips_through_the_strict_installer() {
    let root = tempfile::tempdir().unwrap();
    let original = root.path().join("original");
    let extracted = root.path().join("extracted");
    programs(&original, &build("1.2.3"));
    std::fs::create_dir(&extracted).unwrap();
    let archive = root.path().join("release.tar.gz");
    let output = std::process::Command::new("/usr/bin/tar")
        .env("COPYFILE_DISABLE", "1")
        .args(["--format=ustar", "-czf"])
        .arg(&archive)
        .arg("-C")
        .arg(&original)
        .args(PROGRAMS)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let bytes = std::fs::read(archive).unwrap();
    bundle::unpack(&bytes, &format!("{:x}", Sha256::digest(&bytes)), &extracted).unwrap();
    bundle::verify(&extracted, &build("1.2.3")).unwrap();
}
