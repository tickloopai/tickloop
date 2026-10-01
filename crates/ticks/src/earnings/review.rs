use rust_decimal::Decimal;
use serde::Serialize;
use serde_json::{Value, json};

use super::{EarningsRow, EarningsTable};
use crate::{dates::iso_date, output::TableData};

pub const COMPARISON_PRIORITY: &[usize] = &[0, 1, 3, 4, 6, 2, 5, 7];
pub const DETAIL_PRIORITY: &[usize] = &[0, 1, 2, 3, 4, 5, 6];
pub const ESTIMATE_NOTE: &str =
    "Provider estimates retrieved now; no verified pre-release snapshot";

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct UpcomingPeriod {
    pub period_id: String,
    pub period: String,
    pub release_date: Option<String>,
    pub release_session: Option<&'static str>,
    pub eps_estimate: Option<Decimal>,
    pub revenue_estimate: Option<Decimal>,
}

pub fn upcoming_period(inputs: &super::EarningsInputs) -> Option<UpcomingPeriod> {
    let period = inputs
        .periods
        .iter()
        .filter(|period| !period.is_released)
        .min_by_key(|period| {
            (
                period.fiscal_year,
                super::normalized_fiscal_period(&period.fiscal_period),
            )
        })?;
    let event = inputs.next_report.as_ref().filter(|event| {
        event.fiscal_year == Some(period.fiscal_year)
            && event.fiscal_period.as_ref().is_some_and(|quarter| {
                super::normalized_fiscal_period(quarter)
                    == super::normalized_fiscal_period(&period.fiscal_period)
            })
    });
    Some(UpcomingPeriod {
        period_id: period_id(period.fiscal_year, &period.fiscal_period),
        period: period.label.clone(),
        release_date: event.map(|event| iso_date(event.date)),
        release_session: event.map(|event| event.session.name()),
        eps_estimate: period.eps.estimate,
        revenue_estimate: period.revenue.estimate,
    })
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct DataAvailability {
    pub dataset: &'static str,
    pub available: bool,
    pub reason: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct StatementItem {
    pub statement: String,
    pub field: String,
    pub name: String,
    pub currency: String,
    pub percent: bool,
    pub value: Option<Decimal>,
    pub reported_yoy_ratio: Option<Decimal>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Segment {
    pub category: &'static str,
    pub name: String,
    pub currency: String,
    pub revenue: Option<Decimal>,
    pub share_percent: Option<Decimal>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Growth {
    pub baseline_period_id: Option<String>,
    pub baseline: Option<Decimal>,
    pub absolute_change: Option<Decimal>,
    pub percent: Option<Decimal>,
    pub unavailable_reason: Option<&'static str>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct MetricDetail {
    pub field: String,
    pub name: String,
    pub unit: String,
    pub actual: Option<Decimal>,
    pub estimate: Option<Decimal>,
    pub surprise: Option<Decimal>,
    pub yoy: Growth,
    pub qoq: Growth,
    pub source: &'static str,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct PeriodMaterial {
    pub filing_id: String,
    pub title: String,
    pub url: String,
    pub form: String,
    pub published_at: Option<String>,
    pub period_id: String,
    pub fiscal_period_end: String,
    pub match_basis: &'static str,
    pub verification: &'static str,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct PeriodDetail {
    pub symbol: String,
    pub period_id: String,
    pub period: String,
    pub fiscal_period_end: Option<String>,
    pub report_date: Option<String>,
    pub release_session: Option<&'static str>,
    pub release_timestamp: Option<String>,
    pub observed_at: Option<String>,
    pub source: &'static str,
    pub estimate_snapshot_at: Option<String>,
    pub estimate_note: &'static str,
    pub growth_method: &'static str,
    pub metrics: Vec<MetricDetail>,
    pub statement_items: Vec<StatementItem>,
    pub segments: Vec<Segment>,
    pub availability: Vec<DataAvailability>,
    pub company_filings: Vec<Value>,
    pub materials: Vec<PeriodMaterial>,
    pub filings_note: &'static str,
}

pub fn period_materials(table: &EarningsTable, row: &EarningsRow) -> Vec<PeriodMaterial> {
    let Some(end) = row.fiscal_period_end.as_deref() else {
        return Vec::new();
    };
    let Some(date) = crate::dates::parse_iso_date(end) else {
        return Vec::new();
    };
    let date_tokens = [
        end.replace('-', ""),
        format!("{:02}{:02}{}", date.month() as u8, date.day(), date.year()),
    ];
    let quarter_token = format!("q{}{}", row.fiscal_period, row.fiscal_year);
    let mut materials = Vec::new();
    for filing in &table.company_filings {
        let name = filing["file_name"].as_str().unwrap_or_default();
        let form = name
            .split(" - ")
            .next()
            .unwrap_or_default()
            .to_ascii_uppercase();
        if !matches!(
            form.as_str(),
            "10-Q" | "10-Q/A" | "10-K" | "10-K/A" | "8-K" | "8-K/A"
        ) {
            continue;
        }
        let periodic = form.starts_with("10-");
        if form.starts_with("10-K") && row.fiscal_period != "4" {
            continue;
        }
        for (index, url) in filing["file_urls"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .enumerate()
        {
            let Ok(parsed) = reqwest::Url::parse(url) else {
                continue;
            };
            if !matches!(parsed.scheme(), "https" | "http") {
                continue;
            }
            let filename = parsed
                .path_segments()
                .and_then(|mut path| path.next_back())
                .unwrap_or_default()
                .to_ascii_lowercase();
            let date_matches = date_tokens.iter().any(|token| filename.contains(token));
            let is_periodic_report = periodic && index == 0 && date_matches;
            let is_results_exhibit = !periodic
                && date_matches
                && filename.contains(&quarter_token)
                && (filename.contains("ex99") || filename.contains("exhibit99"));
            if !is_periodic_report && !is_results_exhibit {
                continue;
            }
            materials.push(PeriodMaterial {
                filing_id: filing["id"].as_str().unwrap_or_default().to_owned(),
                title: filing["title"].as_str().unwrap_or(name).to_owned(),
                url: url.to_owned(),
                form: form.clone(),
                published_at: filing["publish_at"].as_str().map(str::to_owned),
                period_id: row.period_id.clone(),
                fiscal_period_end: end.to_owned(),
                match_basis: if is_periodic_report {
                    "primary_report_filename_matches_reported_fiscal_end"
                } else {
                    "results_exhibit_filename_matches_fiscal_quarter_and_end"
                },
                verification: "metadata_match_body_not_verified",
            });
        }
    }
    materials
}

pub fn period_id(year: i32, quarter: &str) -> String {
    format!("FY{year}-Q{}", super::normalized_fiscal_period(quarter))
}

pub fn attach_statements(table: &mut EarningsTable, statements: &Value) {
    for (kind, statement) in statements.as_object().into_iter().flatten() {
        for indicator in statement
            .get("indicators")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let currency = indicator
                .get("currency")
                .and_then(Value::as_str)
                .unwrap_or("");
            for account in indicator
                .get("accounts")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                let Some(field) = account.get("field").and_then(Value::as_str) else {
                    continue;
                };
                for value in account
                    .get("values")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                {
                    let Some((year, quarter, end)) = super::fiscal_period_entry(value) else {
                        continue;
                    };
                    let Some(row) = table
                        .rows
                        .iter_mut()
                        .find(|row| row.fiscal_year == year && row.fiscal_period == quarter)
                    else {
                        continue;
                    };
                    row.fiscal_period_end = Some(iso_date(end));
                    row.statement_items.push(StatementItem {
                        statement: kind.clone(),
                        field: field.to_owned(),
                        name: account
                            .get("name")
                            .and_then(Value::as_str)
                            .unwrap_or(field)
                            .to_owned(),
                        currency: currency.to_owned(),
                        percent: account
                            .get("percent")
                            .and_then(Value::as_bool)
                            .unwrap_or(false),
                        value: decimal(value.get("value")),
                        reported_yoy_ratio: decimal(value.get("yoy")),
                    });
                }
            }
        }
    }
}

pub fn attach_segments(table: &mut EarningsTable, segments: &Value) {
    for snapshot in segments
        .get("historical")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let report = snapshot.get("report_txt").and_then(Value::as_str);
        let fiscal_period = report.and_then(|report| {
            let (year, quarter) = report.split_once(".Q")?;
            let year = year.parse::<i32>().ok()?;
            let quarter = quarter.parse::<u8>().ok()?;
            (1..=4).contains(&quarter).then_some((year, quarter))
        });
        let Some(row) = table.rows.iter_mut().find(|row| {
            if let Some((year, quarter)) = fiscal_period {
                row.fiscal_year == year && row.fiscal_period == quarter.to_string()
            } else if report.is_some() {
                false
            } else {
                snapshot
                    .get("date")
                    .and_then(Value::as_str)
                    .is_some_and(|date| {
                        row.fiscal_period_end
                            .as_deref()
                            .is_some_and(|end| end.replace('-', "") == date)
                    })
            }
        }) else {
            continue;
        };
        let currency = snapshot
            .get("currency")
            .and_then(Value::as_str)
            .unwrap_or("");
        for (key, category) in [("business", "business"), ("regionals", "regional")] {
            for segment in snapshot
                .get(key)
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                let Some(name) = segment.get("name").and_then(Value::as_str) else {
                    continue;
                };
                row.segments.push(Segment {
                    category,
                    name: name.to_owned(),
                    currency: currency.to_owned(),
                    revenue: decimal(segment.get("value")),
                    share_percent: decimal(segment.get("percent")),
                });
            }
        }
    }
}

fn decimal(value: Option<&Value>) -> Option<Decimal> {
    match value? {
        Value::String(value) => value.parse().ok(),
        Value::Number(value) => value.to_string().parse().ok(),
        _ => None,
    }
}

fn growth(
    current: Option<Decimal>,
    baseline: Option<&EarningsRow>,
    value: Option<Decimal>,
) -> Growth {
    let absolute_change = current
        .zip(value)
        .and_then(|(current, previous)| current.checked_sub(previous));
    let percent = value
        .filter(|previous| *previous > Decimal::ZERO)
        .zip(absolute_change)
        .and_then(|(previous, delta)| {
            delta
                .checked_div(previous)?
                .checked_mul(Decimal::ONE_HUNDRED)
        })
        .map(|percent| percent.round_dp(2));
    Growth {
        baseline_period_id: baseline.map(|row| row.period_id.clone()),
        baseline: value,
        absolute_change,
        percent,
        unavailable_reason: if baseline.is_none() {
            Some("baseline_period_missing")
        } else if current.is_none() || value.is_none() {
            Some("metric_missing")
        } else if value.is_some_and(|value| value <= Decimal::ZERO) {
            Some("baseline_non_positive")
        } else if percent.is_none() {
            Some("numeric_overflow")
        } else {
            None
        },
    }
}

pub fn detail(table: &EarningsTable, selected: usize) -> Option<PeriodDetail> {
    let row = table.rows.get(selected)?;
    let quarter: u8 = row
        .fiscal_period
        .parse()
        .ok()
        .filter(|quarter| (1..=4).contains(quarter))?;
    let previous_year = table.rows.iter().find(|candidate| {
        candidate.fiscal_year == row.fiscal_year - 1 && candidate.fiscal_period == row.fiscal_period
    });
    let (year, previous_quarter) = if quarter == 1 {
        (row.fiscal_year - 1, 4)
    } else {
        (row.fiscal_year, quarter - 1)
    };
    let previous = table.rows.iter().find(|candidate| {
        candidate.fiscal_year == year && candidate.fiscal_period == previous_quarter.to_string()
    });
    let mut metrics = Vec::new();
    for (field, name, actual, estimate, surprise, select) in [
        (
            "eps",
            "EPS",
            row.eps_actual,
            row.eps_estimate,
            row.eps_surprise,
            (|row: &EarningsRow| row.eps_actual) as fn(&EarningsRow) -> Option<Decimal>,
        ),
        (
            "revenue",
            "Revenue",
            row.revenue_actual,
            row.revenue_estimate,
            row.revenue_surprise,
            (|row: &EarningsRow| row.revenue_actual) as fn(&EarningsRow) -> Option<Decimal>,
        ),
    ] {
        metrics.push(MetricDetail {
            field: field.into(),
            name: name.into(),
            unit: "provider units".into(),
            actual,
            estimate,
            surprise,
            yoy: growth(actual, previous_year, previous_year.and_then(select)),
            qoq: growth(actual, previous, previous.and_then(select)),
            source: row.metrics_source,
        });
    }
    for item in &row.statement_items {
        let select = |baseline: Option<&EarningsRow>| {
            baseline?
                .statement_items
                .iter()
                .find(|candidate| {
                    candidate.field == item.field
                        && candidate.statement == item.statement
                        && candidate.currency == item.currency
                        && candidate.percent == item.percent
                })?
                .value
        };
        metrics.push(MetricDetail {
            field: format!("{}:{}", item.statement, item.field),
            name: format!("{} · {}", item.statement, item.name),
            unit: if item.percent {
                "provider percentage units".to_owned()
            } else if !item.currency.is_empty()
                && item.name.contains(&format!("({})", item.currency))
            {
                item.currency.clone()
            } else {
                "provider units".to_owned()
            },
            actual: item.value,
            estimate: None,
            surprise: None,
            yoy: growth(item.value, previous_year, select(previous_year)),
            qoq: growth(item.value, previous, select(previous)),
            source: "financial_report",
        });
    }
    let mut availability = table.availability.clone();
    for (dataset, missing) in [
        ("financial_report", row.statement_items.is_empty()),
        ("business_segments_history", row.segments.is_empty()),
    ] {
        if missing
            && let Some(item) = availability
                .iter_mut()
                .find(|item| item.dataset == dataset && item.available)
        {
            item.available = false;
            item.reason = Some("No data matched this exact fiscal period".to_owned());
        }
    }
    let materials = period_materials(table, row);
    Some(PeriodDetail {
        symbol: table.symbol.clone(),
        period_id: row.period_id.clone(),
        period: row.period.clone(),
        fiscal_period_end: row.fiscal_period_end.clone(),
        report_date: row.report_date.clone(),
        release_session: row.session,
        release_timestamp: None,
        observed_at: table.observed_at.clone(),
        source: table.source,
        estimate_snapshot_at: None,
        estimate_note: ESTIMATE_NOTE,
        growth_method: "(actual - baseline) / baseline * 100; exact fiscal quarter match; percent unavailable when baseline <= 0; absolute_change remains available",
        metrics,
        statement_items: row.statement_items.clone(),
        segments: row.segments.clone(),
        availability,
        company_filings: table
            .company_filings
            .iter()
            .filter(|filing| {
                materials
                    .iter()
                    .any(|material| filing["id"].as_str() == Some(material.filing_id.as_str()))
            })
            .cloned()
            .collect(),
        materials,
        filings_note: "Only report originals whose provider URL identifies this exact fiscal period are listed. Filename metadata is not paragraph verification; filing publication time is not necessarily the earnings release time. Empty materials means no exact match in the provider's available filings.",
    })
}

fn number(value: Option<Decimal>) -> Value {
    value
        .map(|value| json!(value.normalize().to_string()))
        .unwrap_or(Value::Null)
}

fn percentage(growth: &Growth) -> Value {
    growth
        .percent
        .map(|value| json!(format!("{}%", value.normalize())))
        .unwrap_or_else(|| json!("N/A"))
}

pub fn comparison_table(table: &EarningsTable) -> TableData {
    TableData {
        headers: vec![
            "Period",
            "EPS Actual",
            "EPS Estimate",
            "EPS YoY",
            "Revenue Actual",
            "Revenue Estimate",
            "Revenue YoY",
            "Report Date",
        ],
        rows: table
            .rows
            .iter()
            .enumerate()
            .map(|(index, row)| {
                let detail = detail(table, index);
                vec![
                    json!(row.period),
                    number(row.eps_actual),
                    number(row.eps_estimate),
                    detail
                        .as_ref()
                        .map(|detail| percentage(&detail.metrics[0].yoy))
                        .unwrap_or(Value::Null),
                    number(row.revenue_actual),
                    number(row.revenue_estimate),
                    detail
                        .as_ref()
                        .map(|detail| percentage(&detail.metrics[1].yoy))
                        .unwrap_or(Value::Null),
                    json!(row.report_date),
                ]
            })
            .collect(),
    }
}

pub fn detail_table(detail: &PeriodDetail) -> TableData {
    let mut rows: Vec<Vec<Value>> = detail
        .metrics
        .iter()
        .map(|metric| {
            vec![
                json!(metric.name),
                number(metric.actual),
                number(metric.estimate),
                number(metric.surprise),
                percentage(&metric.yoy),
                percentage(&metric.qoq),
                json!(metric.unit),
            ]
        })
        .collect();
    for segment in &detail.segments {
        rows.push(vec![
            json!(format!("{} · {}", segment.category, segment.name)),
            number(segment.revenue),
            Value::Null,
            Value::Null,
            Value::Null,
            Value::Null,
            json!(segment.currency),
        ]);
    }
    TableData {
        headers: vec![
            "Metric", "Actual", "Estimate", "Surprise", "YoY", "QoQ", "Unit",
        ],
        rows,
    }
}
