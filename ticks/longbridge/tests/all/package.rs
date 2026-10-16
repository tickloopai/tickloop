use std::path::Path;

use tick_longbridge::region::{CHINA_MAINLAND_ISSUER, GLOBAL_ISSUER};
use tick_packages::{OAuthIssuer, PackageAuth, PackageManifest};

#[test]
fn the_package_manifest_runs_this_program_at_its_package_version_and_logs_in_with_oauth2() {
    let package = Path::new(env!("CARGO_MANIFEST_DIR"));
    let manifest =
        PackageManifest::parse(&std::fs::read(package.join("tick-package.json")).unwrap()).unwrap();
    assert_eq!(manifest.name, tick_longbridge::PACKAGE);
    assert_eq!(manifest.version, env!("CARGO_PKG_VERSION"));
    assert_eq!(manifest.executable, Path::new("bin/tick-longbridge"));
    assert_eq!(
        manifest.executable.file_name(),
        Path::new(env!("CARGO_BIN_EXE_tick-longbridge")).file_name()
    );
    assert!(manifest.protocol);
    assert_eq!(
        manifest.auth,
        PackageAuth::OAuth2 {
            issuers: vec![
                OAuthIssuer {
                    label: "China mainland".into(),
                    issuer: CHINA_MAINLAND_ISSUER.into(),
                },
                OAuthIssuer {
                    label: "Global".into(),
                    issuer: GLOBAL_ISSUER.into(),
                },
            ],
            scopes: Vec::new(),
        }
    );
    assert!(package.join("skills").is_dir());
}
