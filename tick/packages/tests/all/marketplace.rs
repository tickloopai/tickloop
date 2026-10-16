use serde_json::{Value, json};
use tick_packages::{CURRENT_TARGET, Marketplace, PackageDownload};

const SHA256: &str = "3a80b2cd52667cb5812ca59e6f870d0570d5b36bea3979a1e73e01d97d745ce9";
const OTHER_TARGET: &str = "x86_64-unknown-linux-gnu";

fn marketplace_with(packages: Value) -> Value {
    json!({
        "name": "tickloop",
        "owner": { "name": "Tickloop AI", "url": "https://tickloop.ai" },
        "packages": packages,
    })
}

fn package(name: &str, targets: &[&str]) -> Value {
    let downloads = targets
        .iter()
        .map(|target| {
            (
                target.to_string(),
                json!({ "url": format!("https://example.com/tick-{name}-{target}.tar.gz"), "sha256": SHA256 }),
            )
        })
        .collect::<serde_json::Map<_, _>>();
    json!({
        "name": name,
        "version": "0.2.0",
        "description": format!("The {name} package"),
        "downloads": downloads,
    })
}

fn parse(marketplace: &Value) -> Result<Marketplace, String> {
    Marketplace::parse(marketplace.to_string().as_bytes()).map_err(|error| error.to_string())
}

#[test]
fn the_official_marketplace_without_packages_parses() {
    let official = br#"{
  "name": "tickloop",
  "owner": {
    "name": "Tickloop AI",
    "url": "https://tickloop.ai"
  },
  "packages": []
}
"#;

    let marketplace = Marketplace::parse(official).unwrap();

    assert_eq!(marketplace.name, "tickloop");
    assert_eq!(marketplace.owner.name, "Tickloop AI");
    assert_eq!(
        marketplace.owner.url.as_deref(),
        Some("https://tickloop.ai")
    );
    assert!(marketplace.packages.is_empty());
    assert_eq!(marketplace.package("longbridge"), None);
}

#[test]
fn a_package_offers_the_download_for_this_build_s_target_only() {
    let marketplace = parse(&marketplace_with(json!([
        package("longbridge", &[CURRENT_TARGET, OTHER_TARGET]),
        package("other", &[OTHER_TARGET]),
    ])))
    .unwrap();

    let longbridge = marketplace.package("longbridge").unwrap();
    assert_eq!(longbridge.version, "0.2.0");
    assert_eq!(longbridge.description, "The longbridge package");
    assert_eq!(
        longbridge.current_download(),
        Some(&PackageDownload {
            url: format!("https://example.com/tick-longbridge-{CURRENT_TARGET}.tar.gz"),
            sha256: SHA256.to_owned(),
        })
    );
    assert_eq!(
        marketplace.package("other").unwrap().current_download(),
        None
    );
    assert_eq!(marketplace.package("missing"), None);
    if cfg!(all(target_arch = "aarch64", target_os = "macos")) {
        assert_eq!(CURRENT_TARGET, "aarch64-apple-darwin");
    }
}

#[test]
fn an_invalid_marketplace_is_refused_with_its_reason() {
    let renamed = |field: &str, value: Value| {
        let mut entry = package("sample", &[CURRENT_TARGET]);
        entry[field] = value;
        marketplace_with(json!([entry]))
    };
    let mut without_owner = marketplace_with(json!([]));
    without_owner.as_object_mut().unwrap().remove("owner");
    let mut bad_sha256 = package("sample", &[CURRENT_TARGET]);
    bad_sha256["downloads"][CURRENT_TARGET]["sha256"] = json!("abc");
    let cases = [
        (json!([]), "its JSON is not valid"),
        (
            without_owner,
            "its JSON is not valid: missing field `owner`",
        ),
        (
            json!({ "name": "", "owner": { "name": "Tickloop AI" }, "packages": [] }),
            "its name must hold 1 to 500 characters",
        ),
        (
            renamed("name", json!("Sample")),
            "\"Sample\" is not a Tick package name",
        ),
        (
            marketplace_with(json!([
                package("sample", &[CURRENT_TARGET]),
                package("sample", &[OTHER_TARGET]),
            ])),
            "it lists sample more than once",
        ),
        (
            renamed("version", json!("1.0 beta")),
            "version \"1.0 beta\" of sample may hold only",
        ),
        (
            renamed("description", json!("one\nline")),
            "the description of sample must not hold control characters",
        ),
        (
            marketplace_with(json!([bad_sha256])),
            "download of sample has no valid SHA-256",
        ),
    ];

    for (marketplace, reason) in cases {
        let error = parse(&marketplace).unwrap_err();
        assert!(error.contains(reason), "{marketplace}: {error}");
    }
    assert!(Marketplace::parse(b"<html>").is_err());
}
