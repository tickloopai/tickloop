use std::path::Path;

use serde_json::{Value, json};
use tick_packages::{PackageManifest, validate_package_name};

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
    declared["credentials"] = json!("~/.tickloop/longbridge/");
    declared["homepage"] = json!("https://tickloop.ai");

    assert_eq!(
        parse(&declared).unwrap(),
        PackageManifest {
            name: "longbridge".into(),
            version: "0.2.0".into(),
            description: "The longbridge sample".into(),
            executable: "bin/tick-longbridge".into(),
            protocol: true,
            credentials: Some("~/.tickloop/longbridge/".into()),
        }
    );
    assert_eq!(
        parse(&with("protocol", json!(false))).unwrap().credentials,
        None
    );
}

#[test]
fn every_field_but_credentials_is_required() {
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
        ("credentials", ""),
        ("credentials", "~/.tickloop/\u{7}"),
    ] {
        let error = parse(&with(field, json!(text))).unwrap_err();
        assert!(error.contains(field), "{field} {text:?}: {error}");
    }
    let error = parse(&with("description", json!("x".repeat(501)))).unwrap_err();
    assert!(error.contains("1 to 500 characters"), "{error}");
}
