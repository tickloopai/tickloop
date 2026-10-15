use std::path::Path;

use tick_longbridge::auth::storage::AuthStorage;
use tick_packages::PackageManifest;

#[test]
fn the_package_manifest_runs_this_program_at_the_workspace_version_and_names_its_credentials() {
    let package = Path::new(env!("CARGO_MANIFEST_DIR"));
    let manifest =
        PackageManifest::parse(&std::fs::read(package.join("tick-package.json")).unwrap()).unwrap();
    assert_eq!(manifest.name, "longbridge");
    assert_eq!(manifest.version, env!("CARGO_PKG_VERSION"));
    assert_eq!(manifest.executable, Path::new("bin/tick-longbridge"));
    assert_eq!(
        manifest.executable.file_name(),
        Path::new(env!("CARGO_BIN_EXE_tick-longbridge")).file_name()
    );
    assert!(manifest.protocol);
    let home = Path::new("/home/user");
    let credentials = manifest.credentials.unwrap();
    assert_eq!(
        home.join(credentials.strip_prefix("~/").unwrap()),
        AuthStorage::in_home(home).directory
    );
    assert!(package.join("skills").is_dir());
}
