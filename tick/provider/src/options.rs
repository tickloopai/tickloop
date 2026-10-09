//! Option-chain observations a broker provides, independent of its SDK.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Expiration {
    pub date: String,
    pub days_to_expiry: Option<i64>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Contract {
    pub symbol: String,
    pub strike: f64,
    pub expiry: String,
    pub side: OptionSide,
    pub standard_type: Option<i64>,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum OptionSide {
    Call,
    Put,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct MarketQuote {
    pub symbol: String,
    pub last: Option<f64>,
    pub bid: Option<f64>,
    pub ask: Option<f64>,
    pub implied_volatility_percent: Option<f64>,
    pub open_interest: Option<u64>,
    pub volume: Option<u64>,
    pub quote_time: Option<String>,
    pub suspended: bool,
}

impl MarketQuote {
    pub fn midpoint(&self) -> Option<f64> {
        let (bid, ask) = (self.bid?, self.ask?);
        (!self.suspended && bid.is_finite() && ask.is_finite() && bid > 0.0 && ask >= bid)
            .then_some((bid + ask) / 2.0)
    }
}

pub fn normalize_symbol(symbol: &str) -> Result<String> {
    let symbol = symbol.trim().to_uppercase();
    let symbol = if let Some(code) = symbol.strip_prefix("US.") {
        format!("{code}.US")
    } else if let Some(code) = symbol.strip_prefix("HK.") {
        format!("{code}.HK")
    } else if !symbol.contains('.') {
        format!(
            "{symbol}.{}",
            if symbol.chars().all(|c| c.is_ascii_digit()) {
                "HK"
            } else {
                "US"
            }
        )
    } else {
        symbol
    };
    let (code, market) = symbol
        .rsplit_once('.')
        .ok_or_else(|| Error::usage("expected US or HK underlying symbol"))?;
    if code.is_empty()
        || !matches!(market, "US" | "HK")
        || !code
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-'))
    {
        return Err(Error::usage(
            "Options supports US and HK underlying symbols, such as AAPL.US or 00700.HK",
        ));
    }
    if market == "HK" && code.chars().all(|c| c.is_ascii_digit()) {
        return Ok(format!("{code:0>5}.HK"));
    }
    Ok(symbol)
}

#[async_trait]
pub trait OptionsApi: Send + Sync {
    fn source(&self) -> &'static str;
    async fn expirations(&self, symbol: &str) -> Result<Vec<Expiration>>;
    async fn chain(&self, symbol: &str, expiry: &str) -> Result<Vec<Contract>>;
    async fn snapshots(&self, symbols: &[String]) -> Result<Vec<MarketQuote>>;
}
