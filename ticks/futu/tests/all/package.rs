use std::path::{Path, PathBuf};

use tick_packages::{PackageAuth, PackageManifest};

#[test]
fn the_package_manifest_runs_this_program_at_its_package_version() {
    let package = PathBuf::from(
        std::env::var_os("CARGO_MANIFEST_DIR").expect("Cargo sets the test package directory"),
    );
    let manifest =
        PackageManifest::parse(&std::fs::read(package.join("tick-package.json")).unwrap()).unwrap();
    assert_eq!(manifest.name, "futu");
    assert_eq!(manifest.version, env!("CARGO_PKG_VERSION"));
    assert_eq!(manifest.executable, Path::new("bin/tick-futu"));
    assert_eq!(
        manifest.executable.file_name(),
        Path::new(env!("CARGO_BIN_EXE_tick-futu")).file_name()
    );
    assert!(manifest.protocol);
    assert_eq!(manifest.auth, PackageAuth::None);
    assert!(package.join("skills").is_dir());
}
