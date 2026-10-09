use async_trait::async_trait;
use tick_market::{
    args::{OnceArgs, OutputFormat, QuoteArgs},
    commands::quote::run_once,
    config::Settings,
    metadata::TickKind,
};
use tick_provider::{
    api::{Quote, QuoteApi},
    error::Result,
};
use tick_sdk::stream::TickStream;

struct Quotes;
#[async_trait]
impl QuoteApi for Quotes {
    async fn quotes(&self, symbols: &[String]) -> Result<Vec<Quote>> {
        assert_eq!(symbols, &["TSLA.US", "700.HK", "BAD.US"]);
        Ok(serde_json::from_str(r#"[
            {"symbol":"TSLA.US","name":"Tesla","latest_price":"110","previous_close":"100","open":"101","high":"112","low":"99","volume":10000,"turnover":"1100000","updated_at":"2026-09-10T14:00:00Z"},
            {"symbol":"700.HK","name":"腾讯控股","latest_price":"480","previous_close":"500","volume":20000},
            {"symbol":"BAD.US","error":"Unknown symbol"}
        ]"#).unwrap())
    }
}

#[tokio::test]
async fn quote_once_normalizes_symbols_and_keeps_individual_errors_in_table_and_json() {
    let mut args = QuoteArgs {
        symbols: vec!["TSLA".into(), "700".into(), "BAD.US".into()],
        output: OnceArgs {
            once: true,
            format: OutputFormat::Json,
        },
    };
    let mut output = Vec::new();
    run_once(
        &args,
        &Quotes,
        &Settings::default(),
        false,
        &mut output,
        &mut TickStream::in_memory(TickKind::Quote, "longbridge"),
    )
    .await
    .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&output).unwrap();
    assert_eq!(json[0]["symbol"], "TSLA.US");
    assert_eq!(json[0]["change"], "10");
    assert_eq!(json[0]["change_percent"], "10");
    assert_eq!(json[1]["change_percent"], "-4");
    assert!(
        json[1]["open"].is_null(),
        "unavailable opening price stays unknown"
    );
    assert_eq!(json[2]["error"], "Unknown symbol");
    assert!(json[2]["latest_price"].is_null());
    args.output.format = OutputFormat::Table;
    output.clear();
    run_once(
        &args,
        &Quotes,
        &Settings::default(),
        false,
        &mut output,
        &mut TickStream::in_memory(TickKind::Quote, "longbridge"),
    )
    .await
    .unwrap();
    let text = String::from_utf8(output).unwrap();
    assert!(text.contains("Latest Price"), "{text}");
    assert!(text.contains("腾讯控股"), "{text}");
    assert!(text.contains("Unknown symbol"), "{text}");
    assert!(text.contains("1.00万"), "{text}");
    assert!(!text.contains('\u{1b}'));
}

struct SmallPrices;
#[async_trait]
impl QuoteApi for SmallPrices {
    async fn quotes(&self, _: &[String]) -> Result<Vec<Quote>> {
        Ok(serde_json::from_str(r#"[{"symbol":"PENNY.US","name":"Penny","latest_price":"0.001","previous_close":"0.002"}]"#).unwrap())
    }
}
#[tokio::test]
async fn quote_table_preserves_meaningful_subcent_prices() {
    let args = QuoteArgs {
        symbols: vec!["PENNY".into()],
        output: OnceArgs {
            once: true,
            format: OutputFormat::Table,
        },
    };
    let mut output = Vec::new();
    run_once(
        &args,
        &SmallPrices,
        &Settings::default(),
        false,
        &mut output,
        &mut TickStream::in_memory(TickKind::Quote, "longbridge"),
    )
    .await
    .unwrap();
    let text = String::from_utf8(output).unwrap();
    assert!(text.contains("0.001"), "{text}");
    assert!(text.contains("-0.001"), "{text}");
}

struct Unavailable;
#[async_trait]
impl QuoteApi for Unavailable {
    async fn quotes(&self, _: &[String]) -> Result<Vec<Quote>> {
        Err(tick_provider::error::Error::network(
            "quote service unavailable",
        ))
    }
}
#[tokio::test]
async fn whole_service_failure_returns_network_exit_code_without_partial_output() {
    let args = QuoteArgs {
        symbols: vec!["TSLA".into()],
        output: OnceArgs {
            once: true,
            format: OutputFormat::Json,
        },
    };
    let mut output = Vec::new();
    let error = run_once(
        &args,
        &Unavailable,
        &Settings::default(),
        false,
        &mut output,
        &mut TickStream::in_memory(TickKind::Quote, "longbridge"),
    )
    .await
    .unwrap_err();
    assert_eq!(error.code, 4);
    assert!(output.is_empty());
}

#[test]
fn tick_updates_use_the_canonical_versioned_schema() {
    let mut updates = TickStream::in_memory(TickKind::Quote, "demo");
    updates
        .publish(serde_json::json!({"symbol":"TSLA.US","last":"421.50"}))
        .unwrap();

    let update: serde_json::Value = updates.records().all().remove(0);
    assert_eq!(update["version"], 3);
    assert_eq!(update["tick"], "quote");
    assert!(
        update["tick_instance_id"]
            .as_str()
            .unwrap()
            .starts_with("herdr:")
    );
    assert_eq!(update["source"], "demo");
    assert_eq!(update["sequence"], 1);
    assert_eq!(update["kind"], "snapshot");
    assert!(update["observed_at"].as_str().unwrap().ends_with('Z'));
    assert_eq!(
        update["data"],
        serde_json::json!({"symbol":"TSLA.US","last":"421.50"})
    );
    assert!(update.get("module").is_none());
    assert!(update.get("instance_id").is_none());
}
