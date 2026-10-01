//! Read-only entitlement probe for the fundamentals and calendar calls the Earnings Tick needs.
//!
//! Every call is a GET against the Longbridge quote endpoints; nothing is written to the account.
//! A call that fails is recorded with its exact error and the remaining calls still run, so one
//! missing entitlement never hides the shape of the other responses.

use std::{collections::BTreeSet, path::Path, sync::Arc, time::Duration};

use longbridge::{
    CalendarContext, Config, FundamentalContext,
    calendar::{CalendarCategory, CalendarEventsResponse},
    fundamental::types::{
        ConsensusDetail, ConsensusReport, FinancialConsensus, USKeyFinancialMetrics,
    },
    quote::QuoteContext,
};
use serde::Serialize;
use serde_json::Value;

use crate::{
    cli::ProbeArgs,
    error::{Error, Result},
    rate_limit::RateLimiter,
};

pub const DEFAULT_SYMBOL: &str = "TSLA.US";

/// Read from the workspace lockfile by `build.rs`, so it follows the pinned dependency.
const SDK_REVISION: &str = env!("LONGBRIDGE_SDK_REVISION");
const CALL_TIMEOUT: Duration = Duration::from_secs(30);
const CALENDAR_DAYS_BACK: i64 = 70;
const CALENDAR_DAYS_AHEAD: i64 = 120;
const CALENDAR_MAX_PAGES: usize = 120;
const CANDLE_DAYS_BACK: i64 = 60;
const TRADING_DAYS_BACK: i64 = 30;
const KEY_METRICS_REPORT: &str = "quarterly";
const MISSING: &str = "—";
const MAX_FIELD_DEPTH: usize = 6;
const RAW_EXCERPT_LIMIT: usize = 1200;
const SYMBOL_SAMPLE: usize = 12;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CallOutcome {
    Succeeded {
        fields: Vec<String>,
        findings: Vec<String>,
    },
    Failed {
        error: String,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CallReport {
    pub call: String,
    pub endpoint: String,
    pub request: String,
    pub outcome: CallOutcome,
}

impl CallReport {
    pub fn succeeded(
        call: &str,
        endpoint: &str,
        request: impl Into<String>,
        response: &Value,
        findings: Vec<String>,
    ) -> Self {
        Self {
            call: call.to_owned(),
            endpoint: endpoint.to_owned(),
            request: request.into(),
            outcome: CallOutcome::Succeeded {
                fields: field_names(response),
                findings,
            },
        }
    }

    pub fn failed(
        call: &str,
        endpoint: &str,
        request: impl Into<String>,
        error: impl Into<String>,
    ) -> Self {
        Self {
            call: call.to_owned(),
            endpoint: endpoint.to_owned(),
            request: request.into(),
            outcome: CallOutcome::Failed {
                error: error.into(),
            },
        }
    }
}

#[derive(Clone, Debug)]
pub struct ProbeReport {
    pub symbol: String,
    pub data_center: String,
    pub probed_at: String,
    pub calls: Vec<CallReport>,
}

struct PlannedColumn {
    column: &'static str,
    call: &'static str,
    field: &'static str,
    fallback: Option<&'static str>,
    note: Option<&'static str>,
}

const PLANNED_COLUMNS: &[PlannedColumn] = &[
    PlannedColumn {
        column: "Report date (planned source)",
        call: "us_key_financial_metrics",
        field: "list[].rpt_date",
        fallback: Some(
            "take the report date from the `finance_calendar` event for the period; `financial_report` carries only `fp_end`, the fiscal period end, not the announcement date",
        ),
        note: None,
    },
    PlannedColumn {
        column: "Report date (probed source)",
        call: "finance_calendar",
        field: "list[].date",
        fallback: None,
        note: Some(
            "`finance_calendar` is market-wide and pages through `next_date` at roughly one page per calendar date, so ticket 02 asks for a short window around each period instead of walking a whole year",
        ),
    },
    PlannedColumn {
        column: "Release session for the earnings-day rule",
        call: "finance_calendar",
        field: "list[].infos[].date_type",
        fallback: Some(
            "leave the release session unknown and print `—` for the earnings-day move rather than assuming one",
        ),
        note: Some(
            "`date_type` carries the session for past events while `financial_market_time` is often empty; `ext.financial_report.market_time` repeats it in a machine-readable form",
        ),
    },
    PlannedColumn {
        column: "Next report date in the title",
        call: "finance_calendar",
        field: "list[].infos[].ext.financial_report.period",
        fallback: None,
        note: Some(
            "the forward window carries type `report` events and may not list the symbol at all yet, so the title leaves the next report date out rather than guessing",
        ),
    },
    PlannedColumn {
        column: "Fiscal period label",
        call: "consensus",
        field: "list[].period_text",
        fallback: None,
        note: Some(
            "`consensus` orders periods newest first, `fiscal_period` is the bare quarter number and `period_text` is the label to show; unreleased periods come back with `is_released` false and must be dropped before the ten rows are taken",
        ),
    },
    PlannedColumn {
        column: "EPS estimate",
        call: "consensus",
        field: "list[].details[].estimate",
        fallback: None,
        note: None,
    },
    PlannedColumn {
        column: "EPS actual",
        call: "consensus",
        field: "list[].details[].actual",
        fallback: None,
        note: None,
    },
    PlannedColumn {
        column: "EPS surprise",
        call: "consensus",
        field: "list[].details[].comp_value",
        fallback: None,
        note: Some(
            "`comp_value` is actual minus estimate and is absent whenever either side is, so a missing cell prints `—` and leaves the beat counts alone",
        ),
    },
    PlannedColumn {
        column: "Revenue estimate",
        call: "consensus",
        field: "list[].details[].estimate",
        fallback: None,
        note: None,
    },
    PlannedColumn {
        column: "Revenue actual",
        call: "consensus",
        field: "list[].details[].actual",
        fallback: None,
        note: None,
    },
    PlannedColumn {
        column: "Revenue surprise",
        call: "consensus",
        field: "list[].details[].comp_value",
        fallback: None,
        note: None,
    },
    PlannedColumn {
        column: "Trading-day offsets for the ±5 and ±20 day windows",
        call: "trading_days",
        field: "trading_days",
        fallback: Some("derive the offsets from the candlestick dates themselves"),
        note: Some(
            "`trading_days` rejects a long range with `301600 too many query days`, so ticket 02 asks for about a month at a time",
        ),
    },
    PlannedColumn {
        column: "Window closes and the magnitude bar",
        call: "history_candlesticks_by_date",
        field: "[].close",
        fallback: None,
        note: Some(
            "only completed sessions come back, so a post-earnings window that has not finished yet prints `—` instead of a partial move",
        ),
    },
    PlannedColumn {
        column: "Cross-check for estimate and actual on the report date",
        call: "finance_calendar",
        field: "list[].infos[].data_kv[].type",
        fallback: None,
        note: Some(
            "past calendar events carry `estimate_eps`, `actual_eps`, `estimate_revenue` and `actual_revenue` in `data_kv`, which ticket 02 can use to verify a row without a second `consensus` call",
        ),
    },
];

pub fn field_names(response: &Value) -> Vec<String> {
    let mut names = Vec::new();
    collect_field_names(response, "", 1, &mut names);
    names.sort();
    names.dedup();
    names
}

fn collect_field_names(value: &Value, prefix: &str, depth: usize, names: &mut Vec<String>) {
    match value {
        Value::Object(entries) if depth <= MAX_FIELD_DEPTH => {
            for (key, child) in entries {
                let path = if prefix.is_empty() {
                    key.clone()
                } else {
                    format!("{prefix}.{key}")
                };
                collect_field_names(child, &path, depth + 1, names);
                names.push(path);
            }
        }
        Value::Array(items) => {
            if let Some(first) = items.first() {
                collect_field_names(first, &format!("{prefix}[]"), depth, names);
            }
        }
        _ => (),
    }
}

pub fn consensus_findings(response: &FinancialConsensus) -> Vec<String> {
    let keys: BTreeSet<&str> = response
        .list
        .iter()
        .flat_map(|period| period.details.iter())
        .map(|detail| detail.key.as_str())
        .collect();
    let released = response
        .list
        .iter()
        .filter(|period| period.details.iter().any(|detail| detail.is_released))
        .count();
    let mut lines = vec![
        format!(
            "- `current_period` {}, `currency` {}, `opt_periods` {}",
            response.current_period,
            response.currency,
            joined(response.opt_periods.iter().map(String::as_str)),
        ),
        format!(
            "- {} periods returned, `current_index` {}",
            response.list.len(),
            response.current_index,
        ),
        format!("- `details[].key`: {}", joined(keys.into_iter())),
        format!("- {released} of {} periods released", response.list.len()),
        String::new(),
        "| Period | Fiscal year | Fiscal period | Released | EPS estimate | EPS actual | EPS surprise | Revenue estimate | Revenue actual |".into(),
        "| --- | --- | --- | --- | --- | --- | --- | --- | --- |".into(),
    ];
    for period in &response.list {
        let eps = detail(period, "eps");
        let revenue = detail(period, "revenue");
        lines.push(format!(
            "| {} | {} | {} | {} | {} | {} | {} | {} | {} |",
            period.period_text,
            period.fiscal_year,
            period.fiscal_period,
            if eps.is_some_and(|detail| detail.is_released) {
                "yes"
            } else {
                "no"
            },
            decimal(eps.and_then(|detail| detail.estimate)),
            decimal(eps.and_then(|detail| detail.actual)),
            decimal(eps.and_then(|detail| detail.comp_value)),
            decimal(revenue.and_then(|detail| detail.estimate)),
            decimal(revenue.and_then(|detail| detail.actual)),
        ));
    }
    lines
}

pub fn key_metrics_findings(response: &USKeyFinancialMetrics) -> Vec<String> {
    let field_keys: BTreeSet<&str> = response
        .list
        .iter()
        .flat_map(|period| period.fields.iter())
        .filter_map(Value::as_object)
        .flat_map(|entry| entry.keys())
        .map(String::as_str)
        .collect();
    let mut lines = vec![
        format!(
            "- `report` {}, `currency` {}",
            response.report, response.currency
        ),
        format!("- {} periods returned", response.list.len()),
        format!(
            "- `empty_fields`: {}",
            joined(response.empty_fields.iter().map(String::as_str))
        ),
        format!("- `fields[]` keys: {}", joined(field_keys.into_iter())),
        String::new(),
        "| Report | Fiscal year | Fiscal period | Period end | rpt_date |".into(),
        "| --- | --- | --- | --- | --- |".into(),
    ];
    for period in &response.list {
        lines.push(format!(
            "| {} | {} | {} | {} | {} |",
            displayed(&period.report_txt),
            period.ff_year,
            displayed(&period.ff_period),
            displayed(&period.fp_end),
            displayed(&period.rpt_date),
        ));
    }
    lines
}

pub fn calendar_findings(
    response: &CalendarEventsResponse,
    symbol: &str,
    anchor_date: &str,
) -> Vec<String> {
    let events: usize = response.list.iter().map(|group| group.infos.len()).sum();
    let types: BTreeSet<&str> = response
        .list
        .iter()
        .flat_map(|group| group.infos.iter())
        .map(|info| info.event_type.as_str())
        .collect();
    let symbols: BTreeSet<&str> = response
        .list
        .iter()
        .flat_map(|group| group.infos.iter())
        .map(|info| info.symbol.as_str())
        .collect();
    let mut matches: Vec<(&str, &longbridge::calendar::CalendarEventInfo)> = response
        .list
        .iter()
        .flat_map(|group| {
            group
                .infos
                .iter()
                .filter(|info| info.symbol == symbol)
                .map(move |info| (group.date.as_str(), info))
        })
        .collect();
    matches.sort_by_key(|(date, _)| *date);

    let mut lines = vec![
        format!(
            "- window {} onwards; `next_date` {}",
            displayed(&response.date),
            displayed(&response.next_date)
        ),
        format!(
            "- {events} events in the window across {} dates",
            response.list.len()
        ),
        format!("- `list[].infos[].type`: {}", joined(types.into_iter())),
        format!(
            "- {} distinct symbols in the window, first {SYMBOL_SAMPLE}: {}",
            symbols.len(),
            joined(symbols.iter().take(SYMBOL_SAMPLE).copied())
        ),
    ];
    let Some((latest_date, _)) = matches.last() else {
        lines.push(format!("- no `{symbol}` report event in this window"));
        return lines;
    };
    match matches.iter().find(|(date, _)| *date >= anchor_date) {
        Some((next_date, next)) => lines.push(format!(
            "- next `{symbol}` report: {next_date}, `date_type` {}, `financial_market_time` {}, `datetime` {}",
            displayed(&next.date_type),
            displayed(&next.financial_market_time),
            displayed(&next.datetime),
        )),
        None => lines.push(format!(
            "- no `{symbol}` report event at or after {anchor_date}; the latest in the window is {latest_date}"
        )),
    }
    let estimate_types: BTreeSet<&str> = matches
        .iter()
        .flat_map(|(_, info)| info.data_kv.iter())
        .map(|entry| entry.value_type.as_str())
        .collect();
    lines.push(format!(
        "- `data_kv[].type`: {}",
        joined(estimate_types.into_iter())
    ));
    lines.push(format!(
        "- {} `{symbol}` report events, {} before {anchor_date}",
        matches.len(),
        matches
            .iter()
            .filter(|(date, _)| *date < anchor_date)
            .count(),
    ));
    lines.push(String::new());
    lines.push(
        "| Date | date_type | financial_market_time | ext.financial_report.market_time | ext period | infos[].date |"
            .into(),
    );
    lines.push("| --- | --- | --- | --- | --- | --- |".into());
    for (date, info) in &matches {
        lines.push(format!(
            "| {date} | {} | {} | {} | {} | {} |",
            displayed(&info.date_type),
            displayed(&info.financial_market_time),
            displayed(&financial_report_field(info, "market_time")),
            displayed(&financial_report_period(info)),
            displayed(&info.date),
        ));
    }
    lines
}

fn financial_report_field(info: &longbridge::calendar::CalendarEventInfo, key: &str) -> String {
    info.ext
        .as_ref()
        .and_then(|ext| ext.get("financial_report"))
        .and_then(|report| report.get(key))
        .map(|value| {
            value
                .as_str()
                .map_or_else(|| value.to_string(), str::to_owned)
        })
        .unwrap_or_default()
}

fn financial_report_period(info: &longbridge::calendar::CalendarEventInfo) -> String {
    let year = financial_report_field(info, "fiscal_year");
    let period_type = financial_report_field(info, "period_type");
    let period = financial_report_field(info, "period");
    format!("{year} {period_type}{period}").trim().to_owned()
}

/// Fenced JSON for a response whose shape the SDK does not type, truncated so the report stays
/// readable while still showing the field names the endpoint actually returned.
pub fn excerpt(response: &Value, limit: usize) -> Vec<String> {
    let text = serde_json::to_string_pretty(response).unwrap_or_else(|error| error.to_string());
    let body = if text.chars().count() > limit {
        format!(
            "{}\n… truncated",
            text.chars().take(limit).collect::<String>()
        )
    } else {
        text
    };
    vec!["```json".to_owned(), body, "```".to_owned()]
}

pub fn markdown(report: &ProbeReport) -> String {
    let mut lines = vec![
        "# Earnings Tick probe — Longbridge fundamentals and calendar".to_owned(),
        String::new(),
        format!(
            "Symbol `{}` · data center `{}` · probed `{}` · SDK `longbridge rev {SDK_REVISION}`.",
            report.symbol, report.data_center, report.probed_at
        ),
        "Read-only: every call is a GET against the quote endpoints and nothing is written to the account.".to_owned(),
        String::new(),
        "## Status".to_owned(),
        String::new(),
        "| Call | Result |".to_owned(),
        "| --- | --- |".to_owned(),
    ];
    for call in &report.calls {
        lines.push(format!("| {} | {} |", call.call, status(&call.outcome)));
    }
    for call in &report.calls {
        lines.push(String::new());
        lines.push(format!("## {}", call.call));
        lines.push(String::new());
        lines.push(format!("- endpoint `{}`", call.endpoint));
        lines.push(format!("- request `{}`", call.request));
        lines.push(format!("- result: {}", status(&call.outcome)));
        match &call.outcome {
            CallOutcome::Failed { error } => lines.push(format!("- error: `{error}`")),
            CallOutcome::Succeeded { fields, findings } => {
                lines.push(format!(
                    "- fields returned: {}",
                    joined(fields.iter().map(String::as_str))
                ));
                lines.push(String::new());
                lines.extend(findings.iter().cloned());
            }
        }
    }
    lines.push(String::new());
    lines.push("## Column decisions for ticket 02".to_owned());
    lines.push(String::new());
    lines.extend(decision_notes(&report.calls));
    lines.push("| Column | Source call | Field | Decision |".to_owned());
    lines.push("| --- | --- | --- | --- |".to_owned());
    for planned in PLANNED_COLUMNS {
        lines.push(format!(
            "| {} | `{}` | `{}` | {} |",
            planned.column,
            planned.call,
            planned.field,
            decision(&report.calls, planned)
        ));
    }
    lines.push(String::new());
    lines.join("\n")
}

fn decision_notes(calls: &[CallReport]) -> Vec<String> {
    let mut reported = BTreeSet::new();
    let mut notes = Vec::new();
    for planned in PLANNED_COLUMNS {
        let blocked = calls.iter().any(|call| {
            call.call == planned.call && matches!(call.outcome, CallOutcome::Failed { .. })
        });
        if let Some(fallback) = planned.fallback
            && blocked
            && reported.insert(fallback)
        {
            notes.push(format!(
                "- `{}` is unavailable on this authorization, so ticket 02 must {fallback}.",
                planned.call
            ));
        }
        let succeeded = calls.iter().any(|call| {
            call.call == planned.call && matches!(call.outcome, CallOutcome::Succeeded { .. })
        });
        if let Some(note) = planned.note
            && succeeded
            && reported.insert(note)
        {
            notes.push(format!("- {note}."));
        }
    }
    if !notes.is_empty() {
        notes.push(String::new());
    }
    notes
}

fn decision(calls: &[CallReport], planned: &PlannedColumn) -> String {
    let Some(call) = calls.iter().find(|call| call.call == planned.call) else {
        return format!("not probed — `{}` was not run", planned.call);
    };
    match &call.outcome {
        CallOutcome::Failed { error } => {
            let decision = format!("blocked — {}: {error}", planned.call);
            match planned.fallback {
                Some(fallback) => format!("{decision}; {fallback}"),
                None => decision,
            }
        }
        CallOutcome::Succeeded { fields, .. } => {
            if fields.iter().any(|name| name == planned.field) {
                "confirmed".to_owned()
            } else {
                format!("field `{}` missing from the response", planned.field)
            }
        }
    }
}

fn status(outcome: &CallOutcome) -> &'static str {
    match outcome {
        CallOutcome::Succeeded { .. } => "ok",
        CallOutcome::Failed { .. } => "failed",
    }
}

fn detail<'a>(period: &'a ConsensusReport, key: &str) -> Option<&'a ConsensusDetail> {
    period.details.iter().find(|detail| detail.key == key)
}

fn decimal(value: Option<rust_decimal::Decimal>) -> String {
    value.map_or_else(|| MISSING.to_owned(), |value| value.to_string())
}

fn displayed(value: &str) -> String {
    if value.trim().is_empty() {
        MISSING.to_owned()
    } else {
        value.replace('|', "\\|")
    }
}

fn joined<'a>(values: impl Iterator<Item = &'a str>) -> String {
    let joined = values.collect::<Vec<_>>().join(", ");
    if joined.is_empty() {
        "none".to_owned()
    } else {
        joined
    }
}

pub async fn run(
    args: &ProbeArgs,
    config: Arc<Config>,
    limiter: Arc<RateLimiter>,
    data_center: &str,
) -> Result<()> {
    let symbol = &args.symbol;
    let today = args
        .as_of
        .unwrap_or_else(|| time::OffsetDateTime::now_utc().date());
    let fundamental = FundamentalContext::new(config.clone());
    let calendar = CalendarContext::new(config.clone());
    let (quote, _pushes) = QuoteContext::new(config);

    let calls = vec![
        probe_consensus(&fundamental, &limiter, symbol).await,
        probe_key_metrics(&fundamental, &limiter, symbol).await,
        probe_finance_calendar(&calendar, &limiter, symbol, today).await,
        probe_candlesticks(&quote, &limiter, symbol, today).await,
        probe_trading_days(&quote, &limiter, today).await,
        probe_financial_report(&fundamental, &limiter, symbol).await,
    ];
    let report = ProbeReport {
        symbol: symbol.clone(),
        data_center: data_center.to_owned(),
        probed_at: time::OffsetDateTime::now_utc()
            .format(&time::format_description::well_known::Rfc3339)
            .unwrap_or_default(),
        calls,
    };
    write_report(&markdown(&report), args.out.as_deref())
}

fn write_report(report: &str, out: Option<&Path>) -> Result<()> {
    match out {
        Some(path) => {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(path, report)?;
            eprintln!("tick: probe report written to {}", path.display());
            Ok(())
        }
        None => {
            use std::io::Write;
            std::io::stdout()
                .write_all(report.as_bytes())
                .map_err(Error::from)
        }
    }
}

async fn attempt<T, F, Fut>(limiter: &RateLimiter, request: F) -> std::result::Result<T, String>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = longbridge::Result<T>>,
{
    match tokio::time::timeout(CALL_TIMEOUT, limiter.execute(request)).await {
        Ok(Ok(value)) => Ok(value),
        Ok(Err(error)) => Err(error.to_string()),
        Err(_) => Err(format!("timed out after {}s", CALL_TIMEOUT.as_secs())),
    }
}

fn as_json(value: &impl Serialize) -> Value {
    serde_json::to_value(value).unwrap_or(Value::Null)
}

async fn probe_consensus(
    fundamental: &FundamentalContext,
    limiter: &RateLimiter,
    symbol: &str,
) -> CallReport {
    const CALL: &str = "consensus";
    const ENDPOINT: &str = "GET /v1/quote/financial-consensus-detail";
    let request = format!("symbol={symbol}");
    match attempt(limiter, || fundamental.consensus(symbol)).await {
        Ok(response) => CallReport::succeeded(
            CALL,
            ENDPOINT,
            request,
            &as_json(&response),
            consensus_findings(&response),
        ),
        Err(error) => CallReport::failed(CALL, ENDPOINT, request, error),
    }
}

async fn probe_key_metrics(
    fundamental: &FundamentalContext,
    limiter: &RateLimiter,
    symbol: &str,
) -> CallReport {
    const CALL: &str = "us_key_financial_metrics";
    const ENDPOINT: &str = "GET /v1/us/stock-info/fin-keyfactor";
    let request = format!("symbol={symbol}, report={KEY_METRICS_REPORT}");
    match attempt(limiter, || {
        fundamental.us_key_financial_metrics(symbol, KEY_METRICS_REPORT)
    })
    .await
    {
        Ok(response) => CallReport::succeeded(
            CALL,
            ENDPOINT,
            request,
            &as_json(&response),
            key_metrics_findings(&response),
        ),
        Err(error) => CallReport::failed(CALL, ENDPOINT, request, error),
    }
}

async fn probe_finance_calendar(
    calendar: &CalendarContext,
    limiter: &RateLimiter,
    symbol: &str,
    today: time::Date,
) -> CallReport {
    const CALL: &str = "finance_calendar";
    const ENDPOINT: &str = "GET /v1/quote/finance_calendar";
    let market = symbol.rsplit_once('.').map(|(_, market)| market.to_owned());
    let start = calendar_date(today - time::Duration::days(CALENDAR_DAYS_BACK));
    let end = calendar_date(today + time::Duration::days(CALENDAR_DAYS_AHEAD));
    let anchor = calendar_date(today);

    let describe = |pages: usize| {
        format!(
            "category=report, date={start}, date_end={end}, markets[]={}, pages={pages}",
            market.as_deref().unwrap_or("all")
        )
    };
    let mut merged: Option<CalendarEventsResponse> = None;
    let mut pages = 0;
    let mut cursor = start.clone();
    loop {
        let page = attempt(limiter, || {
            calendar.finance_calendar(
                CalendarCategory::Report,
                cursor.as_str(),
                end.as_str(),
                market.clone(),
            )
        })
        .await;
        let page = match page {
            Ok(page) => page,
            Err(error) => {
                return CallReport::failed(CALL, ENDPOINT, describe(pages + 1), error);
            }
        };
        pages += 1;
        let next_cursor = page.next_date.clone();
        match &mut merged {
            None => merged = Some(page),
            Some(merged) => {
                merged.list.extend(page.list);
                merged.next_date = page.next_date;
            }
        }
        // Paging is market-wide, so walk on until the symbol has been seen and the cursor has
        // moved past today: that is the earliest point at which its next report date is known.
        let seen = merged.as_ref().is_some_and(|response| {
            response
                .list
                .iter()
                .any(|group| group.infos.iter().any(|info| info.symbol == symbol))
        });
        if (seen && next_cursor > anchor)
            || next_cursor.is_empty()
            || next_cursor >= end
            || pages >= CALENDAR_MAX_PAGES
        {
            break;
        }
        cursor = next_cursor;
    }

    let response = merged.expect("the loop records a response or returns a failure");
    let mut findings = vec![format!(
        "- {pages} pages walked from {start}; the page cursor reached {}",
        displayed(&response.next_date)
    )];
    findings.extend(calendar_findings(&response, symbol, &anchor));
    CallReport::succeeded(
        CALL,
        ENDPOINT,
        describe(pages),
        &as_json(&response),
        findings,
    )
}

async fn probe_financial_report(
    fundamental: &FundamentalContext,
    limiter: &RateLimiter,
    symbol: &str,
) -> CallReport {
    const CALL: &str = "financial_report";
    const ENDPOINT: &str = "GET /v1/quote/financial-reports (additional: the report date the blocked US endpoint would have carried)";
    let request = format!("symbol={symbol}, kind=IS, report=qf");
    match attempt(limiter, || {
        fundamental.financial_report(
            symbol,
            longbridge::fundamental::types::FinancialReportKind::IncomeStatement,
            Some(longbridge::fundamental::types::FinancialReportPeriod::QuarterlyFull),
        )
    })
    .await
    {
        Ok(response) => {
            let mut findings =
                vec!["- `list` is raw JSON; the excerpt below is its first 1200 characters".into()];
            findings.extend(excerpt(&response.list, RAW_EXCERPT_LIMIT));
            CallReport::succeeded(CALL, ENDPOINT, request, &as_json(&response), findings)
        }
        Err(error) => CallReport::failed(CALL, ENDPOINT, request, error),
    }
}

async fn probe_candlesticks(
    quote: &QuoteContext,
    limiter: &RateLimiter,
    symbol: &str,
    today: time::Date,
) -> CallReport {
    const CALL: &str = "history_candlesticks_by_date";
    const ENDPOINT: &str = "quote websocket cmd GET_SECURITY_HISTORY_CANDLESTICKS (by date)";
    let start = today - time::Duration::days(CANDLE_DAYS_BACK);
    let request = format!(
        "symbol={symbol}, period=day, adjust=no_adjust, start={}, end={}, sessions=intraday",
        calendar_date(start),
        calendar_date(today)
    );
    match attempt(limiter, || {
        quote.history_candlesticks_by_date(
            symbol,
            longbridge::quote::Period::Day,
            longbridge::quote::AdjustType::NoAdjust,
            Some(start),
            Some(today),
            longbridge::quote::TradeSessions::Intraday,
        )
    })
    .await
    {
        Ok(candles) => {
            let findings = vec![
                format!("- {} daily candles returned", candles.len()),
                format!(
                    "- first {}, last {}",
                    candle_date(candles.first()),
                    candle_date(candles.last())
                ),
            ];
            CallReport::succeeded(CALL, ENDPOINT, request, &as_json(&candles), findings)
        }
        Err(error) => CallReport::failed(CALL, ENDPOINT, request, error),
    }
}

async fn probe_trading_days(
    quote: &QuoteContext,
    limiter: &RateLimiter,
    today: time::Date,
) -> CallReport {
    const CALL: &str = "trading_days";
    const ENDPOINT: &str = "quote websocket cmd GET_TRADING_DAYS";
    let start = today - time::Duration::days(TRADING_DAYS_BACK);
    let request = format!(
        "market=US, begin={}, end={}",
        calendar_date(start),
        calendar_date(today)
    );
    match attempt(limiter, || {
        quote.trading_days(longbridge::Market::US, start, today)
    })
    .await
    {
        Ok(days) => {
            let findings = vec![
                format!(
                    "- {} trading days and {} half trading days returned",
                    days.trading_days.len(),
                    days.half_trading_days.len()
                ),
                format!(
                    "- first {}, last {}",
                    days.trading_days
                        .first()
                        .map_or(MISSING.to_owned(), |day| calendar_date(*day)),
                    days.trading_days
                        .last()
                        .map_or(MISSING.to_owned(), |day| calendar_date(*day)),
                ),
            ];
            CallReport::succeeded(CALL, ENDPOINT, request, &as_json(&days), findings)
        }
        Err(error) => CallReport::failed(CALL, ENDPOINT, request, error),
    }
}

fn calendar_date(date: time::Date) -> String {
    format!(
        "{:04}-{:02}-{:02}",
        date.year(),
        date.month() as u8,
        date.day()
    )
}

fn candle_date(candle: Option<&longbridge::quote::Candlestick>) -> String {
    candle.map_or(MISSING.to_owned(), |candle| {
        calendar_date(candle.timestamp.date())
    })
}
