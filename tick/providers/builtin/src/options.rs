use async_trait::async_trait;
use tick_provider::{
    error::Result,
    options::{Contract, Expiration, MarketQuote, OptionSide, OptionsApi},
};

pub struct DemoOptions;

#[async_trait]
impl OptionsApi for DemoOptions {
    fn source(&self) -> &'static str {
        crate::SOURCE
    }
    async fn expirations(&self, _: &str) -> Result<Vec<Expiration>> {
        Ok([1, 8, 29]
            .into_iter()
            .map(|days| Expiration {
                date: tick_provider::dates::iso_date(
                    time::OffsetDateTime::now_utc().date() + time::Duration::days(days),
                ),
                days_to_expiry: Some(days),
            })
            .collect())
    }
    async fn chain(&self, symbol: &str, expiry: &str) -> Result<Vec<Contract>> {
        Ok((80..=120)
            .step_by(5)
            .flat_map(|strike| {
                [("C", OptionSide::Call), ("P", OptionSide::Put)].map(move |(suffix, side)| {
                    Contract {
                        symbol: format!("DEMO{}{}{}.US", expiry.replace('-', ""), strike, suffix),
                        strike: strike as f64,
                        expiry: expiry.into(),
                        side,
                        standard_type: Some(1),
                    }
                })
            })
            .collect::<Vec<_>>()
            .into_iter()
            .map(|mut contract| {
                if symbol.ends_with(".HK") {
                    contract.symbol = contract.symbol.replace(".US", ".HK");
                }
                contract
            })
            .collect())
    }
    async fn snapshots(&self, symbols: &[String]) -> Result<Vec<MarketQuote>> {
        let timestamp = time::OffsetDateTime::now_utc()
            .format(&time::format_description::well_known::Rfc3339)
            .unwrap_or_default();
        Ok(symbols
            .iter()
            .map(|symbol| {
                let contract =
                    symbol.starts_with("DEMO") && (symbol.contains("C.") || symbol.contains("P."));
                let call = symbol.contains("C.");
                let midpoint = if call { 3.1 } else { 2.9 };
                MarketQuote {
                    symbol: symbol.clone(),
                    last: Some(if contract { midpoint } else { 100.0 }),
                    bid: Some(if contract { midpoint - 0.1 } else { 99.9 }),
                    ask: Some(if contract { midpoint + 0.1 } else { 100.1 }),
                    implied_volatility_percent: contract.then_some(88.0),
                    open_interest: contract.then_some(1250),
                    volume: Some(if contract { 420 } else { 2_000_000 }),
                    quote_time: Some(timestamp.clone()),
                    suspended: false,
                }
            })
            .collect())
    }
}
