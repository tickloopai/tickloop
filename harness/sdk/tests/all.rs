use std::path::Path;

use tickloop_harness::tools::is_sensitive_path;

/// Hosts that route or list files outside the file tools refuse the same credential paths.
#[test]
fn hosts_classify_credential_paths_as_the_file_tools_do() {
    for path in [
        "/home/user/.ssh/id_ed25519",
        "project/.env.local",
        "deploy/service-credentials.json",
        "certs/server.PEM",
    ] {
        assert!(is_sensitive_path(Path::new(path)), "{path}");
    }
    for path in [
        "/design/index.html",
        "src/environment.rs",
        "notes/keynote.md",
    ] {
        assert!(!is_sensitive_path(Path::new(path)), "{path}");
    }
}
