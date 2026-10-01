//! Option-chain observations and local research snapshots, independent of a broker SDK.

use std::{io::Write, path::PathBuf};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use time::{OffsetDateTime, PrimitiveDateTime, format_description::well_known::Rfc3339};
use time_tz::{OffsetDateTimeExt, PrimitiveDateTimeExt};

use crate::{
    error::{Error, Result},
    research::{ResearchContext, ResearchStatus},
};

pub const DEFAULT_STRIKES: usize = 21;
pub const MAX_STRIKES: usize = 100;
pub const MAX_HISTORY: usize = 100;
pub const MAX_SNAPSHOT_BYTES: u64 = 2 * 1024 * 1024;

pub fn quote_instant(value: &str, timezone: &str) -> Option<OffsetDateTime> {
    if let Ok(instant) = OffsetDateTime::parse(value, &Rfc3339) {
        return Some(instant);
    }
    let format = time::macros::format_description!("[year]-[month]-[day] [hour]:[minute]:[second]");
    let local = PrimitiveDateTime::parse(value, format).ok()?;
    if timezone == "UTC" {
        return Some(local.assume_utc());
    }
    local
        .assume_timezone(time_tz::timezones::get_by_name(timezone)?)
        .take()
}

fn snapshot_time_bounds(snapshot: &OptionsSnapshot) -> Option<(OffsetDateTime, OffsetDateTime)> {
    let observed = OffsetDateTime::parse(&snapshot.observed_at, &Rfc3339).ok()?;
    let mut earliest = observed;
    let mut latest = observed;
    for quote in std::iter::once(&snapshot.underlying).chain(
        snapshot
            .rows
            .iter()
            .flat_map(|row| [&row.call, &row.put])
            .filter_map(|leg| leg.as_ref()?.quote.as_ref()),
    ) {
        let instant = quote_instant(quote.quote_time.as_deref()?, &snapshot.quote_time_zone)?;
        if instant > observed {
            return None;
        }
        earliest = earliest.min(instant);
        latest = latest.max(instant);
    }
    Some((earliest, latest))
}

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

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct OptionLeg {
    pub contract: Contract,
    pub quote: Option<MarketQuote>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct StrikePair {
    pub strike: f64,
    pub call: Option<OptionLeg>,
    pub put: Option<OptionLeg>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct StraddleEstimate {
    pub strike: f64,
    pub expiry: String,
    pub underlying_price: f64,
    pub underlying_quote_time: Option<String>,
    pub call_symbol: String,
    pub put_symbol: String,
    pub call_midpoint: f64,
    pub put_midpoint: f64,
    pub call_quote_time: Option<String>,
    pub put_quote_time: Option<String>,
    pub premium: f64,
    pub move_percent: f64,
    pub method: String,
    pub interpretation: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct OptionsSnapshot {
    pub schema_version: u32,
    pub source: String,
    pub symbol: String,
    pub expiry: String,
    pub expirations: Vec<Expiration>,
    pub observed_at: String,
    pub quote_time_zone: String,
    pub underlying: MarketQuote,
    pub rows: Vec<StrikePair>,
    pub atm: Option<StraddleEstimate>,
    pub warnings: Vec<String>,
    #[serde(default)]
    pub research_context: Option<ResearchContext>,
}

impl OptionsSnapshot {
    pub fn bind_event(&mut self, context: Option<&ResearchContext>) {
        self.research_context = context.cloned().map(|mut context| {
            context.documents.clear();
            context.evidence = None;
            context
        });
    }
}

#[derive(Clone, Debug)]
pub struct OptionsRequest {
    pub symbol: String,
    pub expiry: Option<String>,
    pub strikes: usize,
}

impl OptionsRequest {
    pub fn new(symbol: &str, expiry: Option<&str>, strikes: usize) -> Result<Self> {
        if !(1..=MAX_STRIKES).contains(&strikes) {
            return Err(Error::usage(format!(
                "strikes must be between 1 and {MAX_STRIKES}"
            )));
        }
        let symbol = normalize_symbol(symbol)?;
        if let Some(expiry) = expiry
            && crate::dates::parse_iso_date(expiry)
                .is_none_or(|date| crate::dates::iso_date(date) != expiry)
        {
            return Err(Error::usage("expiry must be a valid YYYY-MM-DD date"));
        }
        Ok(Self {
            symbol,
            expiry: expiry.map(str::to_owned),
            strikes,
        })
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
    fn source(&self) -> &'static str {
        "futu"
    }
    async fn expirations(&self, symbol: &str) -> Result<Vec<Expiration>>;
    async fn chain(&self, symbol: &str, expiry: &str) -> Result<Vec<Contract>>;
    async fn snapshots(&self, symbols: &[String]) -> Result<Vec<MarketQuote>>;
}

pub async fn load(api: &dyn OptionsApi, request: &OptionsRequest) -> Result<OptionsSnapshot> {
    let mut expirations = api.expirations(&request.symbol).await?;
    expirations.retain(|date| date.days_to_expiry.is_none_or(|days| days >= 0));
    expirations.sort_by(|a, b| a.date.cmp(&b.date));
    expirations.dedup_by(|a, b| a.date == b.date);
    let expiry = match &request.expiry {
        Some(expiry) if expirations.iter().any(|date| &date.date == expiry) => expiry.clone(),
        Some(expiry) => {
            return Err(Error::usage(format!(
                "expiry {expiry} is not returned by {} for {}",
                api.source(),
                request.symbol
            )));
        }
        None => expirations
            .first()
            .map(|date| date.date.clone())
            .ok_or_else(|| {
                Error::network("OpenD returned no unexpired option expirations for this symbol")
            })?,
    };
    let preliminary = api.snapshots(std::slice::from_ref(&request.symbol)).await?;
    let price = preliminary
        .iter()
        .find(|quote| quote.symbol == request.symbol)
        .and_then(|quote| quote.last)
        .filter(|price| price.is_finite() && *price > 0.0)
        .ok_or_else(|| {
            Error::network(
                "underlying snapshot has no positive price; cannot select near-the-money strikes",
            )
        })?;
    let contracts = api.chain(&request.symbol, &expiry).await?;
    let mut strikes: Vec<_> = contracts
        .iter()
        .filter(|contract| {
            contract.expiry == expiry
                && contract.strike.is_finite()
                && contract.strike > 0.0
                && contract.standard_type != Some(2)
        })
        .map(|contract| contract.strike)
        .collect();
    strikes.sort_by(f64::total_cmp);
    strikes.dedup();
    let all_strikes = strikes.clone();
    strikes.sort_by(|a, b| {
        (a - price)
            .abs()
            .total_cmp(&(b - price).abs())
            .then(a.total_cmp(b))
    });
    strikes.truncate(request.strikes);
    strikes.sort_by(f64::total_cmp);
    let contracts: Vec<_> = contracts
        .into_iter()
        .filter(|contract| {
            contract.expiry == expiry
                && strikes.contains(&contract.strike)
                && contract.standard_type != Some(2)
        })
        .collect();
    let mut symbols = vec![request.symbol.clone()];
    symbols.extend(contracts.iter().map(|contract| contract.symbol.clone()));
    symbols.sort();
    symbols.dedup();
    if symbols.len() > MAX_STRIKES * 2 + 1 {
        return Err(Error::network(
            "option chain has ambiguous duplicate contracts; refusing to mix adjusted or incompatible legs",
        ));
    }
    let quotes = api.snapshots(&symbols).await?;
    let underlying = quotes
        .iter()
        .find(|quote| quote.symbol == request.symbol)
        .cloned()
        .ok_or_else(|| {
            Error::network("OpenD omitted the underlying from the final quote snapshot")
        })?;
    let leg = |strike, side| {
        let matching: Vec<_> = contracts
            .iter()
            .filter(|contract| contract.strike == strike && contract.side == side)
            .collect();
        if matching.len() != 1 {
            return None;
        }
        let contract = matching[0].clone();
        let quote = quotes
            .iter()
            .find(|quote| quote.symbol == contract.symbol)
            .cloned();
        Some(OptionLeg { contract, quote })
    };
    let rows: Vec<_> = strikes
        .into_iter()
        .map(|strike| StrikePair {
            strike,
            call: leg(strike, OptionSide::Call),
            put: leg(strike, OptionSide::Put),
        })
        .collect();
    let nearest_is_loaded = underlying.last.is_some_and(|price| {
        all_strikes
            .iter()
            .min_by(|a, b| (*a - price).abs().total_cmp(&(*b - price).abs()))
            .is_some_and(|nearest| rows.iter().any(|row| row.strike == *nearest))
    });
    let atm = nearest_is_loaded
        .then(|| straddle(&underlying, &rows, &expiry))
        .flatten();
    let mut warnings = vec!["Snapshot quote times use exchange-local time (US: Eastern, HK: Hong Kong); observed_at is UTC. Legs can have different quote times; freshness is not guaranteed.".into(), "ATM straddle premium / underlying price is a pricing approximation, not a direction forecast, probability interval, or pure earnings-event volatility.".into()];
    if atm.is_none() {
        warnings.push("ATM move unavailable: missing/ambiguous call-put pair, nonstandard contract, suspended security, missing quote time, or absent/zero/crossed bid-ask. Last trades are not substituted for bid-ask midpoints.".into());
    }
    if !nearest_is_loaded && !rows.is_empty() {
        warnings.push("Underlying moved outside the requested strike window while loading; refresh before calculating a new ATM estimate.".into());
    }
    if rows.is_empty() {
        warnings.push("No standard option contracts were returned for this expiry.".into());
    }
    Ok(OptionsSnapshot {
        schema_version: 1,
        source: api.source().into(),
        symbol: request.symbol.clone(),
        expiry,
        expirations,
        observed_at: now_string(),
        quote_time_zone: if api.source() == "demo" {
            "UTC"
        } else if request.symbol.ends_with(".US") {
            "America/New_York"
        } else {
            "Asia/Hong_Kong"
        }
        .into(),
        underlying,
        rows,
        atm,
        warnings,
        research_context: None,
    })
}

pub fn straddle(
    underlying: &MarketQuote,
    rows: &[StrikePair],
    expiry: &str,
) -> Option<StraddleEstimate> {
    let price = underlying
        .last
        .filter(|price| price.is_finite() && *price > 0.0)?;
    if underlying.suspended || underlying.quote_time.as_deref().is_none_or(str::is_empty) {
        return None;
    }
    let row = rows.iter().min_by(|a, b| {
        (a.strike - price)
            .abs()
            .total_cmp(&(b.strike - price).abs())
    })?;
    let (call, put) = (row.call.as_ref()?, row.put.as_ref()?);
    if call.contract.expiry != expiry
        || put.contract.expiry != expiry
        || call.contract.strike != put.contract.strike
        || call.contract.standard_type != Some(1)
        || put.contract.standard_type != Some(1)
    {
        return None;
    }
    let (call_quote, put_quote) = (call.quote.as_ref()?, put.quote.as_ref()?);
    if call_quote.quote_time.as_deref().is_none_or(str::is_empty)
        || put_quote.quote_time.as_deref().is_none_or(str::is_empty)
    {
        return None;
    }
    let (call_midpoint, put_midpoint) = (call_quote.midpoint()?, put_quote.midpoint()?);
    let premium = call_midpoint + put_midpoint;
    Some(StraddleEstimate { strike:row.strike, expiry:expiry.into(), underlying_price:price, underlying_quote_time:underlying.quote_time.clone(), call_symbol:call.contract.symbol.clone(), put_symbol:put.contract.symbol.clone(), call_midpoint, put_midpoint, call_quote_time:call_quote.quote_time.clone(), put_quote_time:put_quote.quote_time.clone(), premium, move_percent:premium/price*100.0, method:"(ATM call bid-ask midpoint + ATM put bid-ask midpoint) / underlying last price * 100".into(), interpretation:"Approximate priced magnitude through expiry, not direction, a guaranteed range, or event-only variance. Quote timestamps are retained and may differ.".into() })
}

pub fn now_string() -> String {
    time::OffsetDateTime::now_utc()
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_default()
}

#[derive(Clone)]
pub struct SnapshotStore {
    pub directory: PathBuf,
}

#[derive(Debug, Serialize)]
pub struct SnapshotHistory {
    pub snapshots: Vec<OptionsSnapshot>,
    pub missing: bool,
    pub invalid_files: Vec<String>,
    pub note: &'static str,
}

#[derive(Clone, Debug, Serialize)]
pub struct EventSnapshotHistory {
    pub research_context: ResearchContext,
    pub before: Option<OptionsSnapshot>,
    pub after: Option<OptionsSnapshot>,
    pub unclassified_count: usize,
    pub invalid_files: Vec<String>,
    pub saved_spot_change_percent: Option<f64>,
    pub comparison_basis: &'static str,
    pub timing_basis: &'static str,
}

fn event_prefix(context: &ResearchContext) -> Result<String> {
    use sha1::{Digest, Sha1};
    let symbol = normalize_symbol(&context.symbol)?;
    let key = format!(
        "{symbol}:{}:{}",
        context.period_id,
        context.release_date.as_deref().unwrap_or("unknown")
    );
    Ok(format!(
        "{symbol}-event-{:x}-",
        Sha1::digest(key.as_bytes())
    ))
}

impl SnapshotStore {
    pub fn new(directory: Option<PathBuf>) -> Result<Self> {
        let directory = directory
            .or_else(|| {
                crate::config::default_path()
                    .and_then(|path| path.parent().map(|parent| parent.join("snapshots/options")))
            })
            .ok_or_else(|| {
                Error::usage("cannot determine snapshot directory; pass --snapshot-dir")
            })?;
        Ok(Self { directory })
    }

    pub fn save(&self, snapshot: &OptionsSnapshot) -> Result<PathBuf> {
        let symbol = normalize_symbol(&snapshot.symbol)?;
        let prefix = if let Some(context) = &snapshot.research_context {
            context.validate()?;
            if normalize_symbol(&context.symbol)? != symbol {
                return Err(Error::usage("snapshot and research context symbols differ"));
            }
            event_prefix(context)?
        } else {
            format!("{symbol}-")
        };
        std::fs::create_dir_all(&self.directory)?;
        let mut file = tempfile::NamedTempFile::new_in(&self.directory)?;
        serde_json::to_writer(&mut file, snapshot)
            .map_err(|error| Error::usage(error.to_string()))?;
        file.flush()?;
        file.as_file().sync_all()?;
        let name = format!(
            "{prefix}{:020}-{}.json",
            time::OffsetDateTime::now_utc().unix_timestamp_nanos(),
            std::process::id()
        );
        let path = self.directory.join(name);
        file.persist_noclobber(&path).map_err(|error| {
            Error::usage(format!("cannot save options snapshot: {}", error.error))
        })?;
        Ok(path)
    }

    pub fn history(&self, symbol: &str, limit: usize) -> Result<SnapshotHistory> {
        let symbol = normalize_symbol(symbol)?;
        if !(1..=MAX_HISTORY).contains(&limit) {
            return Err(Error::usage(format!(
                "history limit must be between 1 and {MAX_HISTORY}"
            )));
        }
        self.read_history(&symbol, &format!("{symbol}-"), limit)
    }

    fn read_history(&self, symbol: &str, prefix: &str, limit: usize) -> Result<SnapshotHistory> {
        let mut history = SnapshotHistory {
            snapshots: vec![],
            missing: true,
            invalid_files: vec![],
            note: "Only snapshots explicitly saved by this Tick are available. Missing pre-event history is not reconstructed from current option prices.",
        };
        let entries = match std::fs::read_dir(&self.directory) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(history),
            Err(error) => return Err(error.into()),
        };
        let mut paths = entries
            .filter_map(std::result::Result::ok)
            .map(|entry| entry.path())
            .filter(|path| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.starts_with(prefix) && name.ends_with(".json"))
            })
            .collect::<Vec<_>>();
        paths.sort_by(|a, b| b.cmp(a));
        for path in paths.into_iter().take(limit) {
            let parsed = (|| -> std::result::Result<OptionsSnapshot, String> {
                let metadata =
                    std::fs::symlink_metadata(&path).map_err(|error| error.to_string())?;
                if !metadata.is_file() || metadata.len() > MAX_SNAPSHOT_BYTES {
                    return Err("not a bounded regular snapshot file".into());
                }
                let snapshot: OptionsSnapshot = serde_json::from_slice(
                    &std::fs::read(&path).map_err(|error| error.to_string())?,
                )
                .map_err(|error| error.to_string())?;
                if snapshot.schema_version != 1 || snapshot.symbol != symbol {
                    return Err("snapshot schema or symbol mismatch".into());
                }
                Ok(snapshot)
            })();
            match parsed {
                Ok(snapshot) => history.snapshots.push(snapshot),
                Err(error) => history
                    .invalid_files
                    .push(format!("{}: {error}", path.display())),
            }
        }
        history.missing = history.snapshots.is_empty();
        Ok(history)
    }

    pub fn event_history(&self, context: &ResearchContext) -> Result<EventSnapshotHistory> {
        context.validate()?;
        let symbol = normalize_symbol(&context.symbol)?;
        let stored = self.read_history(&symbol, &event_prefix(context)?, usize::MAX)?;
        let mut result = EventSnapshotHistory {
            research_context: context.clone(),
            before: None,
            after: None,
            unclassified_count: 0,
            invalid_files: stored.invalid_files,
            saved_spot_change_percent: None,
            comparison_basis: "Change between saved underlying quotes only; not regular-close reaction, not isolated earnings return. Original observation and quote times define the interval.",
            timing_basis: "Exact release time unknown: only observations entirely before the release calendar day or after that day can be classified. Same-day observations remain unclassified.",
        };
        let zone = if symbol.ends_with(".US") {
            time_tz::timezones::db::america::NEW_YORK
        } else {
            time_tz::timezones::db::asia::HONG_KONG
        };
        if let Some(at) = context
            .published_at
            .as_deref()
            .and_then(|at| OffsetDateTime::parse(at, &Rfc3339).ok())
            && context
                .release_date
                .as_deref()
                .and_then(crate::dates::parse_iso_date)
                .is_some_and(|date| at.to_timezone(zone).date() != date)
        {
            return Err(Error::usage(
                "actual publication time does not match the market-local release date",
            ));
        }
        let boundaries = context
            .release_date
            .as_deref()
            .and_then(crate::dates::parse_iso_date)
            .and_then(|date| {
                let start = date.midnight().assume_timezone(zone).take()?;
                let end = date.next_day()?.midnight().assume_timezone(zone).take()?;
                Some((start, end))
            });
        let boundaries = if context.status == ResearchStatus::Released {
            context.published_at.as_deref().and_then(|at| OffsetDateTime::parse(at, &Rfc3339).ok()).map(|at| {
                result.timing_basis = "Actual release instant compared with original observation time and every available underlying/leg quote time.";
                (at, at)
            }).or(boundaries)
        } else {
            result.timing_basis = "Preparation before the scheduled release calendar day; actual publication is not yet confirmed. No after-event observation is claimed.";
            boundaries
        };
        for snapshot in stored.snapshots {
            let matches_event = snapshot.research_context.as_ref().is_some_and(|saved| {
                saved.period_id == context.period_id
                    && saved.release_date == context.release_date
                    && normalize_symbol(&saved.symbol).ok().as_ref() == Some(&symbol)
            });
            if !matches_event {
                continue;
            }
            let phase = snapshot_time_bounds(&snapshot)
                .zip(boundaries)
                .map(|((first, last), (start, end))| {
                    if last < start {
                        -1
                    } else if first >= end && context.status == ResearchStatus::Released {
                        1
                    } else {
                        0
                    }
                })
                .unwrap_or(0);
            let slot = if phase == -1 {
                let date = context
                    .release_date
                    .as_deref()
                    .and_then(crate::dates::parse_iso_date);
                let expiry = crate::dates::parse_iso_date(&snapshot.expiry)
                    .filter(|date| crate::dates::iso_date(*date) == snapshot.expiry);
                let covers_event = expiry.zip(date).is_some_and(|(expiry, date)| {
                    expiry > date
                        || (expiry == date
                            && matches!(
                                context.release_session,
                                crate::event_chart::EventSession::PreMarket
                                    | crate::event_chart::EventSession::DuringMarket
                            ))
                });
                if !covers_event {
                    result.unclassified_count += 1;
                    continue;
                }
                &mut result.before
            } else if phase == 1 {
                &mut result.after
            } else {
                result.unclassified_count += 1;
                continue;
            };
            let observed = OffsetDateTime::parse(&snapshot.observed_at, &Rfc3339).ok();
            let existing = slot
                .as_ref()
                .and_then(|old| OffsetDateTime::parse(&old.observed_at, &Rfc3339).ok());
            if existing.is_none()
                || if phase == -1 {
                    observed > existing
                } else {
                    observed < existing
                }
            {
                *slot = Some(snapshot);
            }
        }
        result.saved_spot_change_percent = result
            .before
            .as_ref()
            .and_then(|snapshot| snapshot.underlying.last)
            .zip(
                result
                    .after
                    .as_ref()
                    .and_then(|snapshot| snapshot.underlying.last),
            )
            .filter(|(before, after)| {
                before.is_finite() && *before > 0.0 && after.is_finite() && *after > 0.0
            })
            .map(|(before, after)| (after / before - 1.0) * 100.0);
        Ok(result)
    }
}
