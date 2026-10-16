use std::{fs, os::unix::fs::PermissionsExt, path::Path};

use tick_packages::{
    ApiKey, OAUTH_TOKEN_FILE_VARIABLE, OAuthIssuer, OAuthToken, PackageAuth, PackageCredentials,
};

fn mode(path: &Path) -> u32 {
    fs::metadata(path).unwrap().permissions().mode() & 0o777
}

fn token() -> OAuthToken {
    OAuthToken {
        access_token: "secret-access".into(),
        token_type: "Bearer".into(),
        refresh_token: None,
        scope: None,
        issuer: "https://auth.example.com".into(),
        client_id: "client-1".into(),
        token_endpoint: "https://auth.example.com/token".into(),
        revocation_endpoint: None,
        expires_at: 1,
    }
}

/// A program starts with what its package's login needs: the token file of an `oauth2` package
/// even before the user logs in, and the key of an `api_key` package once there is one.
#[test]
fn a_program_starts_with_the_credentials_its_package_declares() {
    let directory = tempfile::tempdir().unwrap();
    let config_root = directory.path().join("config");
    let credentials = PackageCredentials::in_config_root(&config_root, "sample").unwrap();
    let oauth2 = PackageAuth::OAuth2 {
        issuers: vec![OAuthIssuer {
            label: "Server".into(),
            issuer: "https://auth.example.com".into(),
        }],
        scopes: Vec::new(),
    };
    let api_key = PackageAuth::ApiKey {
        env: "SAMPLE_KEY".into(),
        label: "Sample key".into(),
    };

    assert_eq!(credentials.launch_environment(&PackageAuth::None), []);
    let token_file = config_root.join("credentials/sample/oauth2.json");
    assert_eq!(
        credentials.launch_environment(&oauth2),
        [(
            OAUTH_TOKEN_FILE_VARIABLE.to_owned(),
            token_file.to_str().unwrap().to_owned()
        )]
    );
    assert_eq!(
        credentials.launch_environment(&api_key),
        [],
        "a program without a key starts without one"
    );
    credentials
        .save_api_key(&ApiKey::new("sample-secret").unwrap())
        .unwrap();
    assert_eq!(
        credentials.launch_environment(&api_key),
        [("SAMPLE_KEY".to_owned(), "sample-secret".to_owned())]
    );
    std::fs::write(credentials.api_key_path(), "\u{7}").unwrap();
    assert_eq!(
        credentials.launch_environment(&api_key),
        [],
        "a key that cannot be read is no key"
    );
}

/// A relative config root still names the token file by its absolute path.
#[test]
fn the_token_file_is_named_by_its_absolute_path() {
    let credentials = PackageCredentials::in_config_root("relative".as_ref(), "sample").unwrap();
    let oauth2 = PackageAuth::OAuth2 {
        issuers: vec![OAuthIssuer {
            label: "Server".into(),
            issuer: "https://auth.example.com".into(),
        }],
        scopes: Vec::new(),
    };
    let environment = credentials.launch_environment(&oauth2);
    let token_file = std::path::Path::new(&environment[0].1);
    assert!(token_file.is_absolute(), "{token_file:?}");
    assert!(token_file.ends_with("relative/credentials/sample/oauth2.json"));
}

/// A program pointed at a token file in a directory someone else made, such as the home
/// directory, leaves that directory's permissions alone; one it creates is owner-only, as the
/// directory Tickloop picks under the config root always is.
#[test]
fn only_directories_made_here_or_chosen_by_tickloop_become_owner_only() {
    let directory = tempfile::tempdir().unwrap();
    let shared = directory.path().join("shared");
    fs::create_dir(&shared).unwrap();
    fs::set_permissions(&shared, fs::Permissions::from_mode(0o755)).unwrap();
    let pointed = PackageCredentials::with_token_file(&shared.join("oauth2.json")).unwrap();
    pointed.lock_oauth().unwrap().save_token(&token()).unwrap();
    assert_eq!(mode(&shared), 0o755);
    assert_eq!(mode(&pointed.oauth_token_path()), 0o600);
    assert_eq!(pointed.oauth_token().unwrap(), Some(token()));

    let created = directory.path().join("new/login");
    let pointed = PackageCredentials::with_token_file(&created.join("oauth2.json")).unwrap();
    pointed.lock_oauth().unwrap();
    assert_eq!(mode(&created), 0o700);

    let config_root = directory.path().join("config");
    let chosen = config_root.join("credentials/sample");
    fs::create_dir_all(&chosen).unwrap();
    fs::set_permissions(&chosen, fs::Permissions::from_mode(0o755)).unwrap();
    PackageCredentials::in_config_root(&config_root, "sample")
        .unwrap()
        .lock_oauth()
        .unwrap();
    assert_eq!(mode(&chosen), 0o700);
}

#[test]
fn a_program_is_pointed_only_at_an_absolute_oauth2_json() {
    for token_file in [
        "oauth2.json",
        "relative/oauth2.json",
        "/tmp/token.json",
        "/",
    ] {
        let error = PackageCredentials::with_token_file(Path::new(token_file)).unwrap_err();
        assert!(
            error.to_string().contains(OAUTH_TOKEN_FILE_VARIABLE),
            "{token_file}: {error}"
        );
    }
}
