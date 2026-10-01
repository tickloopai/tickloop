use async_trait::async_trait;
use longbridge::trade::{USAssetOverview, USStockEntry};
use serde_json::{Value, json};
use tickloop_ticks::{
    api::{Quote, QuoteApi, TradeApi},
    cli::{OnceArgs, OutputFormat},
    commands::positions,
    config::Settings,
    error::Result,
    metadata::TickKind,
    stream::TickStream,
    us_positions,
};

struct UsPortfolio(USAssetOverview);

#[async_trait]
impl TradeApi for UsPortfolio {
    async fn positions(&self) -> Result<Value> {
        us_positions::positions_response(&self.0)
    }
}

struct Quotes;

#[async_trait]
impl QuoteApi for Quotes {
    async fn quotes(&self, symbols: &[String]) -> Result<Vec<Quote>> {
        assert_eq!(symbols, &["AAPL.US"]);
        Ok(serde_json::from_value(json!([
            {"symbol": "AAPL.US", "latest_price": "200"}
        ]))
        .unwrap())
    }
}

fn portfolio(account_type: &str) -> UsPortfolio {
    UsPortfolio(USAssetOverview {
        account_type: account_type.into(),
        currency: "USD".into(),
        cash_buy_power: "PRIVATE_BUYING_POWER".into(),
        stock_list: vec![USStockEntry {
            symbol: "AAPL".into(),
            full_symbol: "AAPL.US".into(),
            name: "Apple Inc.".into(),
            quantity: "2.5".into(),
            average_cost: "160".into(),
            currency: "USD".into(),
            last_done: "199".into(),
            industry_name: "PRIVATE_INDUSTRY".into(),
            ..Default::default()
        }],
        ..Default::default()
    })
}

#[tokio::test]
async fn us_positions_use_fractional_holdings_and_quotes_without_inventing_available_or_account_mode()
 {
    for account_type in ["0", "1", ""] {
        let mut output = Vec::new();
        let mut updates = TickStream::in_memory(TickKind::Positions);
        positions::run_once(
            &OnceArgs {
                once: true,
                format: OutputFormat::Json,
            },
            &Quotes,
            &portfolio(account_type),
            &Settings::default(),
            false,
            &mut output,
            &mut updates,
        )
        .await
        .unwrap();

        let rows: Value = serde_json::from_slice(&output).unwrap();
        assert_eq!(rows[0]["symbol"], "AAPL.US");
        assert_eq!(rows[0]["name"], "Apple Inc.");
        assert_eq!(rows[0]["quantity"], "2.5");
        assert_eq!(rows[0]["available_quantity"], Value::Null);
        assert_eq!(rows[0]["cost_price"], "160");
        assert_eq!(rows[0]["latest_price"], "200");
        assert_eq!(rows[0]["market_value"], "500");
        assert_eq!(rows[0]["profit_loss"], "100");
        assert_eq!(rows[0]["profit_loss_percent"], "25");
        assert_eq!(rows[1]["symbol"], "Total USD");
        assert_eq!(rows[1]["market_value"], "500");
        assert_eq!(rows[0].as_object().unwrap().len(), 9);
        assert!(!String::from_utf8(output).unwrap().contains("PRIVATE"));
        let event: Value = updates.records().all().remove(0);
        assert_eq!(event["data"]["account_mode"], "unknown");
        assert_eq!(event["data"]["positions"], rows);
    }
}

struct NoQuoteRequest;

#[async_trait]
impl QuoteApi for NoQuoteRequest {
    async fn quotes(&self, _: &[String]) -> Result<Vec<Quote>> {
        panic!("invalid or empty holdings must not request quotes")
    }
}

#[tokio::test]
async fn incomplete_us_holdings_fail_before_requesting_quotes_or_publishing_content() {
    for missing_field in ["symbol", "currency", "quantity", "cost"] {
        let mut portfolio = portfolio("0");
        let stock = &mut portfolio.0.stock_list[0];
        match missing_field {
            "symbol" => stock.full_symbol.clear(),
            "currency" => stock.currency.clear(),
            "quantity" => stock.quantity.clear(),
            "cost" => stock.average_cost.clear(),
            _ => unreachable!(),
        }
        let mut output = Vec::new();
        let mut updates = TickStream::in_memory(TickKind::Positions);
        let error = positions::run_once(
            &OnceArgs {
                once: true,
                format: OutputFormat::Json,
            },
            &NoQuoteRequest,
            &portfolio,
            &Settings::default(),
            false,
            &mut output,
            &mut updates,
        )
        .await
        .unwrap_err();
        assert_eq!(error.code, 4);
        assert!(output.is_empty());
        assert!(updates.records().is_empty());
    }
}
