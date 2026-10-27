use serde_json::{Value, json};
use std::time::Instant;

struct Entry {
    secrets: Vec<String>,
}

fn redact(entry: &Entry, content: String) -> String {
    let normalizer = icu_normalizer::ComposingNormalizerBorrowed::new_nfc();
    let mut content = normalizer.normalize(&content).into_owned();
    let secrets = entry.secrets.clone();
    let mut variants = std::collections::BTreeSet::new();
    for secret in secrets.into_iter().filter(|secret| !secret.is_empty()) {
        let secret = normalizer.normalize(&secret).into_owned();
        variants.insert(secret.clone());
        let encoded =
            percent_encoding::utf8_percent_encode(&secret, percent_encoding::NON_ALPHANUMERIC)
                .to_string();
        variants.insert(encoded.clone());
        variants.insert(encoded.replace("%2F", "%2f").replace("%2B", "%2b"));
        variants.insert(secret.bytes().map(|b| format!("%{b:02X}")).collect());
        variants.insert(secret.bytes().map(|b| format!("%{b:02x}")).collect());
        variants.insert(
            serde_json::to_string(&secret)
                .unwrap_or_default()
                .trim_matches('"')
                .to_owned(),
        );
    }
    let mut variants: Vec<_> = variants.into_iter().collect();
    variants.sort_by_key(|value| std::cmp::Reverse(value.len()));
    for secret in variants {
        if !secret.is_empty() {
            content = content.replace(&secret, "[redacted]");
        }
    }
    content
}
fn redact_value(entry: &Entry, value: &mut Value) {
    match value {
        Value::String(text) => *text = redact(entry, std::mem::take(text)),
        Value::Array(values) => {
            for v in values {
                redact_value(entry, v)
            }
        }
        Value::Object(values) => {
            let old = std::mem::take(values);
            for (key, mut v) in old {
                redact_value(entry, &mut v);
                values.insert(redact(entry, key), v);
            }
        }
        _ => {}
    }
}
fn schema() -> Value {
    let mut props = serde_json::Map::new();
    for i in 0..12 {
        props.insert(format!("field_{i}"), json!({"type":"string","description":format!("The field number {i} used by the tool, e.g. 700.HK or AAPL.US; see docs for details."),"enum":["a","b","c"]}));
    }
    json!({"type":"object","properties":props,"required":["field_0"],"additionalProperties":false,"$schema":"http://json-schema.org/draft-07/schema#"})
}
fn main() {
    let tools: usize = std::env::args()
        .nth(1)
        .and_then(|a| a.parse().ok())
        .unwrap_or(200);
    let tok = |n: usize| {
        (0..n)
            .map(|i| (b'a' + (i % 26) as u8) as char)
            .collect::<String>()
    };
    // env secrets (e.g. *_TOKEN vars), OAuth access+refresh token, URL/arg values
    let entry = Entry {
        secrets: vec![
            tok(43),
            tok(40),
            tok(41),
            tok(39),
            tok(44),
            tok(42),
            tok(1580),
            tok(1581),
        ],
    };
    let entry_no_oauth = Entry {
        secrets: vec![tok(43), tok(40), tok(41), tok(39), tok(44), tok(42)],
    };
    for (label, e) in [("oauth", &entry), ("env-only", &entry_no_oauth)] {
        let t = Instant::now();
        let mut n = 0;
        for _ in 0..tools {
            let mut s = schema();
            let mut d = Value::String(
                "Get the latest quote of a security; returns price, volume and turnover.".into(),
            );
            redact_value(e, &mut d);
            redact_value(e, &mut s); // definition schema in catalog()
            let mut s2 = s.clone();
            redact_value(e, &mut s2); // redact_schema again in discovery_tools()
            n += 1;
        }
        println!(
            "{label}: redact for {n} tools (catalog+discovery): {:?}",
            t.elapsed()
        );
    }
    let t = Instant::now();
    for _ in 0..tools {
        let s = schema();
        assert!(jsonschema::validator_for(&s).is_ok());
    }
    println!("jsonschema validator_for x{tools}: {:?}", t.elapsed());
}
