use std::path::Path;

use tick_packages::{OAuthIssuer, PackageAuth, PackageManifest};

#[test]
fn the_package_manifest_runs_this_program_at_the_workspace_version_and_logs_in_with_oauth2() {
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
    assert_eq!(
        manifest.auth,
        PackageAuth::OAuth2 {
            issuers: vec![
                OAuthIssuer {
                    label: "China mainland".into(),
                    issuer: "https://openapi.longbridge.cn".into(),
                },
                OAuthIssuer {
                    label: "Global".into(),
                    issuer: "https://openapi.longbridge.com".into(),
                },
            ],
            scopes: Vec::new(),
        }
    );
    assert!(package.join("skills").is_dir());
}
