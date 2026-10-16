use tick_packages::{
    ApiKey, OAUTH_TOKEN_FILE_VARIABLE, OAuthIssuer, PackageAuth, PackageCredentials,
};

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
