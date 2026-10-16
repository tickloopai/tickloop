use std::path::Path;

use serde_json::{Value, json};
use tick_packages::{OAuthIssuer, PackageAuth, PackageManifest, validate_package_name};

use crate::support::manifest;

fn parse(manifest: &Value) -> Result<PackageManifest, String> {
    PackageManifest::parse(manifest.to_string().as_bytes()).map_err(|error| error.to_string())
}

fn with(field: &str, value: Value) -> Value {
    let mut changed = manifest("longbridge", "0.2.0");
    changed[field] = value;
    changed
}

#[test]
fn a_manifest_declares_the_package_and_its_program() {
    let mut declared = manifest("longbridge", "0.2.0");
    declared["homepage"] = json!("https://tickloop.ai");

    assert_eq!(
        parse(&declared).unwrap(),
        PackageManifest {
            name: "longbridge".into(),
            version: "0.2.0".into(),
            description: "The longbridge sample".into(),
            executable: "bin/tick-longbridge".into(),
            protocol: true,
            auth: PackageAuth::None,
        }
    );
}

fn issuer(label: &str, issuer: &str) -> OAuthIssuer {
    OAuthIssuer {
        label: label.into(),
        issuer: issuer.into(),
    }
}

#[test]
fn a_manifest_declares_no_login_an_api_key_or_oauth2() {
    for none in [json!(null), json!({"type": "none"})] {
        assert_eq!(parse(&with("auth", none)).unwrap().auth, PackageAuth::None);
    }
    assert_eq!(
        parse(&with(
            "auth",
            json!({"type": "api_key", "env": "SAMPLE_API_KEY", "label": "Sample API key"})
        ))
        .unwrap()
        .auth,
        PackageAuth::ApiKey {
            env: "SAMPLE_API_KEY".into(),
            label: "Sample API key".into(),
        }
    );
    assert_eq!(
        parse(&with(
            "auth",
            json!({"type": "oauth2", "issuers": [
                {"label": "China mainland", "issuer": "https://openapi.longbridge.cn"},
                {"label": "Global", "issuer": "https://openapi.longbridge.com"},
            ]})
        ))
        .unwrap()
        .auth,
        PackageAuth::OAuth2 {
            issuers: vec![
                issuer("China mainland", "https://openapi.longbridge.cn"),
                issuer("Global", "https://openapi.longbridge.com"),
            ],
            scopes: Vec::new(),
        }
    );
    assert_eq!(
        parse(&with(
            "auth",
            json!({"type": "oauth2", "scopes": ["4", "quote:read"], "issuers": [
                {"label": "Local", "issuer": "http://127.0.0.1:8080/tenant"},
            ]})
        ))
        .unwrap()
        .auth,
        PackageAuth::OAuth2 {
            issuers: vec![issuer("Local", "http://127.0.0.1:8080/tenant")],
            scopes: vec!["4".into(), "quote:read".into()],
        }
    );

    for not_auth in [
        json!({"login": ["auth"], "status": ["auth", "status", "--json"], "logout": ["auth", "logout"]}),
        json!({"type": "password"}),
        json!({"type": "api_key", "label": "Key"}),
        json!({"type": "api_key", "env": "KEY"}),
        json!({"type": "oauth2"}),
        json!({"type": "oauth2", "issuers": ["https://example.com"]}),
        json!("oauth2"),
    ] {
        let error = parse(&with("auth", not_auth.clone())).unwrap_err();
        assert!(
            error.contains("tick-package.json is not valid"),
            "{not_auth}: {error}"
        );
    }
}

#[test]
fn an_api_key_goes_in_a_variable_that_cannot_change_how_programs_run() {
    let key = |env: &str| {
        with(
            "auth",
            json!({"type": "api_key", "env": env, "label": "Sample key"}),
        )
    };
    for allowed in ["A", "SAMPLE_KEY", "KEY2", "X_1_", "PATHS", "HOMEBREW_TOKEN"] {
        parse(&key(allowed)).unwrap();
    }
    for refused in [
        "",
        "sample_key",
        "Sample",
        "1KEY",
        "_KEY",
        "KEY-1",
        "KEY ONE",
        "KÉY",
        "PATH",
        "HOME",
        "SHELL",
        "USER",
        "TMPDIR",
        "LD_PRELOAD",
        "DYLD_INSERT_LIBRARIES",
        "TICKLOOP_OAUTH_TOKEN_FILE",
        "TICKLOOP_CONFIG_DIR",
    ] {
        let error = parse(&key(refused)).unwrap_err();
        assert!(
            error.contains("auth.env") && error.contains("must match [A-Z][A-Z0-9_]*"),
            "{refused:?}: {error}"
        );
    }
    for label in ["", " ", "two\nlines", "\u{1b}[31m"] {
        let error = parse(&with(
            "auth",
            json!({"type": "api_key", "env": "KEY", "label": label}),
        ))
        .unwrap_err();
        assert!(error.contains("auth.label"), "{label:?}: {error}");
    }
}

#[test]
fn oauth2_issuers_are_distinct_https_urls_and_scopes_are_scope_tokens() {
    let oauth2 = |issuers: Value, scopes: Value| {
        parse(&with(
            "auth",
            json!({"type": "oauth2", "issuers": issuers, "scopes": scopes}),
        ))
    };
    let one = |url: &str| json!([{"label": "Server", "issuer": url}]);
    for allowed in [
        "https://auth.example.com",
        "https://auth.example.com/tenant/one",
        "https://auth.example.com:8443",
        "http://127.0.0.1:9000",
        "http://localhost:9000",
        "http://[::1]:9000",
    ] {
        oauth2(one(allowed), json!([])).unwrap();
    }
    for (refused, problem) in [
        (
            "http://auth.example.com",
            "must use https, or http on this machine",
        ),
        (
            "ftp://auth.example.com",
            "must use https, or http on this machine",
        ),
        ("auth.example.com", "is not a valid URL"),
        (
            "https://user:secret@auth.example.com",
            "must not hold credentials",
        ),
        (
            "https://auth.example.com/?tenant=1",
            "must not have a query or fragment",
        ),
        (
            "https://auth.example.com/#one",
            "must not have a query or fragment",
        ),
    ] {
        let error = oauth2(one(refused), json!([])).unwrap_err();
        assert!(
            error.contains(&format!("issuer {refused} in tick-package.json {problem}")),
            "{refused}: {error}"
        );
    }
    for unprintable in ["https://auth.example\n.com", ""] {
        let error = oauth2(one(unprintable), json!([])).unwrap_err();
        assert!(
            error.contains("each auth.issuers issuer"),
            "{unprintable:?}: {error}"
        );
    }

    let error = oauth2(json!([]), json!([])).unwrap_err();
    assert!(error.contains("must list at least one issuer"), "{error}");
    let error = oauth2(
        json!([
            {"label": "Server", "issuer": "https://one.example.com"},
            {"label": "Server", "issuer": "https://two.example.com"},
        ]),
        json!([]),
    )
    .unwrap_err();
    assert!(error.contains("names the label Server twice"), "{error}");
    let error = oauth2(
        json!([
            {"label": "One", "issuer": "https://one.example.com"},
            {"label": "Two", "issuer": "https://one.example.com"},
        ]),
        json!([]),
    )
    .unwrap_err();
    assert!(
        error.contains("lists https://one.example.com twice"),
        "{error}"
    );
    let error = oauth2(
        json!([{"label": "\u{7}", "issuer": "https://one.example.com"}]),
        json!([]),
    )
    .unwrap_err();
    assert!(error.contains("each auth.issuers label"), "{error}");

    let server = one("https://auth.example.com");
    oauth2(server.clone(), json!(["openid", "read:quotes", "4"])).unwrap();
    for scope in ["", "two words", "quo\"te", "back\\slash", "tab\t", "é"] {
        let error = oauth2(server.clone(), json!([scope])).unwrap_err();
        assert!(
            error.contains("must be printable ASCII without spaces"),
            "{scope:?}: {error}"
        );
    }
}

#[test]
fn every_field_but_auth_is_required() {
    for field in ["name", "version", "description", "executable", "protocol"] {
        let mut incomplete = manifest("longbridge", "0.2.0");
        incomplete.as_object_mut().unwrap().remove(field);
        let error = parse(&incomplete).unwrap_err();
        assert!(
            error.contains(&format!("missing field `{field}`")),
            "{field}: {error}"
        );
    }
    let error = PackageManifest::parse(b"not json").unwrap_err().to_string();
    assert!(error.contains("tick-package.json is not valid"), "{error}");
}

#[test]
fn names_hold_lowercase_letters_digits_and_hyphens() {
    for name in ["longbridge", "futu", "a", "x-1", "9lives"] {
        validate_package_name(name).unwrap();
    }
    for name in [
        "",
        "Longbridge",
        "long_bridge",
        "-futu",
        "a.b",
        "a/b",
        "..",
        &"a".repeat(65),
    ] {
        let error = validate_package_name(name).unwrap_err().to_string();
        assert!(
            error.contains("is not a Tick package name"),
            "{name}: {error}"
        );
    }
    let error = parse(&with("name", json!("Longbridge"))).unwrap_err();
    assert!(error.contains("is not a Tick package name"), "{error}");
}

#[test]
fn the_executable_is_a_relative_path_inside_the_package_named_after_it() {
    assert_eq!(
        parse(&with("executable", json!("tick-longbridge")))
            .unwrap()
            .executable,
        Path::new("tick-longbridge")
    );
    for outside in [
        "",
        "/bin/tick-longbridge",
        "../tick-longbridge",
        "bin/../../tick-longbridge",
        "./bin/tick-longbridge",
    ] {
        let error = parse(&with("executable", json!(outside))).unwrap_err();
        assert!(
            error.contains("must be a relative path inside the package"),
            "{outside}: {error}"
        );
    }
    let error = parse(&with("executable", json!("bin/tick-futu"))).unwrap_err();
    assert!(error.contains("must be named tick-longbridge"), "{error}");
}

#[test]
fn versions_and_texts_are_bounded_and_printable() {
    parse(&with("version", json!("0.2.0-rc.1+build.5"))).unwrap();
    for version in ["", "1.0 beta", "1/0", "v\u{1b}1"] {
        let error = parse(&with("version", json!(version))).unwrap_err();
        assert!(error.contains("version"), "{version:?}: {error}");
    }
    for (field, text) in [
        ("description", ""),
        ("description", "   "),
        ("description", "red \u{1b}[31m text"),
        ("description", "two\nlines"),
    ] {
        let error = parse(&with(field, json!(text))).unwrap_err();
        assert!(error.contains(field), "{field} {text:?}: {error}");
    }
    let error = parse(&with("description", json!("x".repeat(501)))).unwrap_err();
    assert!(error.contains("1 to 500 characters"), "{error}");
}
