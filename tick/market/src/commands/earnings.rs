use std::io::Write;

use crossterm::event::KeyCode;
use futures_util::future::{join, try_join, try_join_all};
use ratatui::{Frame, layout::Rect, widgets::Clear};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Value, json};
use tickloop_ui::{
    format::{Hint, fit_hints, truncate, width, write_text},
    settings::AmbiguousWidth,
};

use crate::{
    args::{EarningsArgs, OutputFormat},
    config::Settings,
    earnings::{
        self, DEFAULT_PERIOD_COUNT, EarningsInputs, EarningsTable, MAX_PERIOD_COUNT, price_windows,
        released_periods, trading_day_windows,
    },
    input::{EditOutcome, TextInput},
    output::TableData,
    research::{
        ResearchContext, ResearchDocument, ResearchEvidence, ResearchNavigation, ResearchStatus,
        ResearchTarget, SetResearchContext,
    },
    runtime::{LiveView, ViewState},
};
use tick_provider::{
    api::{
        CandleAdjust, CandleHistoryRequest, CandlePeriod, ConsensusPeriod, DateRange, MarketEvent,
        QuoteServices, ReportCalendarRequest, ReportDate, ReportEvent, normalize_symbol,
    },
    error::{Error, Result},
};
use tick_sdk::{stream::TickStream, tools::ToolCall};

pub const MARKET: &str = "US";
const HISTORY_TOOL: &str = "history";
const DETAIL_TOOL: &str = "detail";
const SYMBOL_SUFFIX: &str = ".US";
const NON_US_SYMBOL: &str = "Earnings covers US symbols only in this version";
const INFO_SEPARATOR: &str = " · ";

/// The calendar answers one date per page, so a search window costs at most this many pages
/// before the Tick gives up on it.
const CALENDAR_PAGE_BUDGET: usize = 70;

const DAILY_REFRESH_HOUR: u8 = 6;
/// Used only when the refresh hour cannot be turned into an instant, which needs a calendar the
/// `time` crate rejects; a plain day keeps the Panel reloading.
const DAILY_REFRESH_FALLBACK: std::time::Duration = std::time::Duration::from_secs(24 * 60 * 60);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EarningsRequest {
    pub symbol: String,
    pub count: usize,
}

impl EarningsRequest {
    pub fn new(symbol: &str, count: Option<usize>) -> Result<Self> {
        let symbol = normalize_symbol(&symbol.trim().to_uppercase());
        if !symbol.ends_with(SYMBOL_SUFFIX) {
            return Err(Error::usage(format!("{NON_US_SYMBOL}; got {symbol}")));
        }
        let count = count.unwrap_or(DEFAULT_PERIOD_COUNT);
        if !(1..=MAX_PERIOD_COUNT).contains(&count) {
            return Err(Error::usage(format!(
                "count must be between 1 and {MAX_PERIOD_COUNT}"
            )));
        }
        Ok(Self { symbol, count })
    }
}

fn request(args: &EarningsArgs) -> Result<EarningsRequest> {
    EarningsRequest::new(&args.symbol, args.count)
}

/// Arguments of the `history` Tick Tool; the schema the Agent reads is derived from it.
#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct HistoryArgs {
    /// US symbol to read, such as `TSLA.US`.
    pub symbol: String,
    /// How many released periods to return, newest first; ten when left out.
    #[schemars(range(min = 1, max = MAX_PERIOD_COUNT))]
    pub count: Option<usize>,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DetailArgs {
    pub symbol: String,
    /// Stable fiscal identity returned by history, for example FY2026-Q2.
    pub period_id: String,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SelectPeriodArgs {
    /// Stable fiscal identity already loaded in this Tick.
    pub period_id: String,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct EmptyArgs {}

pub fn tools() -> crate::tools::Tools {
    crate::tools::Tools::default()
        .query::<HistoryArgs>(
            HISTORY_TOOL,
            "Past earnings periods of any US symbol: EPS and revenue estimate, actual and surprise, and the moves before, on and after each report, in the Earnings table's shape.",
            |api: &QuoteServices, args| {
                Box::pin(async move {
                    let request = EarningsRequest::new(&args.symbol, args.count)?;
                    content(&load(api, &request, today()).await?)
                })
            },
        )
        .query::<DetailArgs>(
            DETAIL_TOOL,
            "One exact released fiscal period, actuals/estimates, fiscal YoY/QoQ, statement items, business segments, source/time metadata and company filing URLs. Missing historical estimate snapshots remain missing.",
            |api: &QuoteServices, args| {
                Box::pin(async move {
                    let request = EarningsRequest::new(&args.symbol, Some(MAX_PERIOD_COUNT))?;
                    let table = load(api, &request, today()).await?;
                    let selected = find_period(&table, &args.period_id)?;
                    serde_json::to_value(earnings::review::detail(&table, selected))
                        .map_err(|error| Error::usage(error.to_string()))
                })
            },
        )
        .action::<SelectPeriodArgs>(
            "select_period",
            "Open the details of an already-loaded fiscal period in this Earnings Tick; symbol and data are unchanged.",
        )
        .action::<SetResearchContext>("set_context", "Select the company and exact fiscal period carried by an explicit research context.")
        .action::<EmptyArgs>("refresh", "Request a fresh provider observation, preserving the selected fiscal period.")
        .recover_with(HISTORY_TOOL)
}

fn find_period(table: &EarningsTable, period_id: &str) -> Result<usize> {
    table.rows.iter().position(|row| row.period_id == period_id)
        .ok_or_else(|| Error::usage(format!("fiscal period {period_id:?} is not available; query history for valid period_id values")))
}

pub fn today() -> time::Date {
    time::OffsetDateTime::now_utc().date()
}

/// Loads the responses the table needs and assembles them. The newest period is searched first
/// because its announcement teaches how long this issuer takes to report; every other period,
/// the next report, the prices and the trading days are then fetched side by side.
pub async fn load(
    api: &QuoteServices,
    request: &EarningsRequest,
    as_of: time::Date,
) -> Result<EarningsTable> {
    let symbol = request.symbol.as_str();
    let mut periods = api.consensus(symbol).await?;
    let consensus_ids: std::collections::BTreeSet<_> = periods
        .iter()
        .map(|period| earnings::review::period_id(period.fiscal_year, &period.fiscal_period))
        .collect();
    let ((report_dates, period_ends), (statements, (segments, filings))) = join(
        join(
            api.key_financial_metrics(symbol),
            api.fiscal_period_ends(symbol),
        ),
        join(
            api.financial_statements(symbol),
            join(api.business_segments(symbol), api.filings(symbol)),
        ),
    )
    .await;
    let report_dates = report_dates.unwrap_or_default();
    let period_ends = period_ends.unwrap_or_default();

    let covered: Vec<ConsensusPeriod> = released_periods(&periods, request.count)
        .into_iter()
        .cloned()
        .collect();
    let pending: Vec<time::Date> = covered
        .iter()
        .filter(|period| !has_report_date(&report_dates, period))
        .filter_map(|period| earnings::period_end(&period_ends, period))
        .collect();
    let older = earnings::older_period_ends(
        &period_ends,
        &covered,
        request.count.saturating_sub(covered.len()),
        as_of,
    );
    let targets: Vec<time::Date> = pending
        .iter()
        .copied()
        .chain(older.iter().map(|known| known.end))
        .collect();

    let mut lag = earnings::REPORT_LAG_DAYS;
    let mut found = Vec::with_capacity(targets.len());
    if let Some(&newest) = targets.first() {
        let event = search_announcement(api, symbol, newest, lag, as_of).await?;
        if let Some(event) = &event {
            lag = (event.date - newest).whole_days();
        }
        found.push(event);
    }
    let next_window = earnings::next_report_window(&periods, &period_ends, lag, as_of);
    let (rest, next_report) = try_join(
        try_join_all(
            targets
                .iter()
                .skip(1)
                .map(|&end| search_announcement(api, symbol, end, lag, as_of)),
        ),
        search_calendar(api, symbol, next_window, |event| event.date >= as_of),
    )
    .await?;
    found.extend(rest);
    let older_found = found.split_off(pending.len());
    periods.extend(
        older
            .iter()
            .zip(&older_found)
            .filter_map(|(known, event)| Some(earnings::calendar_period(known, event.as_ref()?))),
    );
    let events: Vec<ReportEvent> = found.into_iter().chain(older_found).flatten().collect();

    let announcements: Vec<time::Date> = released_periods(&periods, request.count)
        .into_iter()
        .filter_map(|period| {
            earnings::resolve_announcement(period, &report_dates, &period_ends, &events)
                .map(|(date, _)| date)
        })
        .collect();
    let windows = price_windows(&announcements, as_of);
    let candle_requests: Vec<CandleHistoryRequest> = windows
        .iter()
        .map(|window| CandleHistoryRequest {
            symbol: request.symbol.clone(),
            period: CandlePeriod::Day,
            adjust: CandleAdjust::None,
            range: *window,
        })
        .collect();
    let (candles, trading_days) = try_join(
        try_join_all(
            candle_requests
                .iter()
                .map(|candle_request| api.candles_by_date(candle_request)),
        ),
        try_join_all(
            trading_day_windows(&windows)
                .into_iter()
                .map(|chunk| api.trading_days(MARKET, chunk)),
        ),
    )
    .await?;

    let mut table = earnings::assemble(&EarningsInputs {
        symbol: request.symbol.clone(),
        count: request.count,
        periods,
        report_dates,
        period_ends,
        events,
        next_report,
        trading_days: trading_days.into_iter().flatten().collect(),
        candles: candles.into_iter().flatten().collect(),
    });
    table.source = api.source_name();
    table.observed_at = Some(
        time::OffsetDateTime::now_utc()
            .format(&time::format_description::well_known::Rfc3339)
            .unwrap_or_default(),
    );
    for row in &mut table.rows {
        if !consensus_ids.contains(&row.period_id) {
            row.metrics_source = "finance_calendar";
        }
    }
    for (dataset, result) in [
        ("financial_report", statements),
        ("business_segments_history", segments),
        ("filings", filings),
    ] {
        let available = result.as_ref().is_ok_and(|value| match dataset {
            "financial_report" => value.as_object().is_some_and(|value| !value.is_empty()),
            "business_segments_history" => value
                .get("historical")
                .and_then(Value::as_array)
                .is_some_and(|value| !value.is_empty()),
            _ => value.as_array().is_some_and(|value| !value.is_empty()),
        });
        let reason = match &result {
            Err(error) => Some(error.message.clone()),
            Ok(_) if !available => Some("No data returned for this company".to_owned()),
            _ => None,
        };
        table.availability.push(earnings::review::DataAvailability {
            dataset,
            available,
            reason,
        });
        if let Ok(value) = result {
            match dataset {
                "financial_report" => earnings::review::attach_statements(&mut table, &value),
                "business_segments_history" => {
                    earnings::review::attach_segments(&mut table, &value)
                }
                "filings" => table.company_filings = value.as_array().cloned().unwrap_or_default(),
                _ => unreachable!(),
            }
        }
    }
    Ok(table)
}

fn has_report_date(report_dates: &[ReportDate], period: &ConsensusPeriod) -> bool {
    report_dates.iter().any(|reported| {
        reported.fiscal_year == period.fiscal_year && reported.fiscal_period == period.fiscal_period
    })
}

async fn search_announcement(
    api: &QuoteServices,
    symbol: &str,
    period_end: time::Date,
    lag: i64,
    as_of: time::Date,
) -> Result<Option<ReportEvent>> {
    match earnings::announcement_window(period_end, lag, as_of) {
        Some(window) => search_calendar(api, symbol, window, |_| true).await,
        None => Ok(None),
    }
}

/// Walks the calendar pages inside `window` until the symbol shows up or the budget runs out.
/// Each page holds one date, so the walk stops at the first date that carries the symbol.
async fn search_calendar(
    api: &QuoteServices,
    symbol: &str,
    window: DateRange,
    accept: impl Fn(&ReportEvent) -> bool,
) -> Result<Option<ReportEvent>> {
    let mut cursor = window.start;
    for _ in 0..CALENDAR_PAGE_BUDGET {
        let page = api
            .report_calendar(&ReportCalendarRequest {
                symbol: symbol.to_owned(),
                range: DateRange {
                    start: cursor,
                    end: window.end,
                },
            })
            .await?;
        if let Some(event) = page
            .events
            .into_iter()
            .filter(|event| event.symbol == symbol && accept(event))
            .min_by_key(|event| event.date)
        {
            return Ok(Some(event));
        }
        match page.next_date {
            Some(next) if next > cursor && next <= window.end => cursor = next,
            _ => return Ok(None),
        }
    }
    Ok(None)
}

pub async fn run_once(
    args: &EarningsArgs,
    api: &QuoteServices,
    settings: &Settings,
    color: bool,
    output: &mut impl Write,
    updates: &mut TickStream,
) -> Result<()> {
    let request = request(args)?;
    let table = load(api, &request, today()).await?;
    updates.publish(summary(&table)?)?;
    if args.output.format == OutputFormat::Json {
        serde_json::to_writer(&mut *output, &content(&table)?)
            .map_err(|error| Error::usage(error.to_string()))?;
        writeln!(output)?;
        return Ok(());
    }
    earnings::table(&table).write(OutputFormat::Table, settings, color, output)
}

fn content(table: &EarningsTable) -> Result<serde_json::Value> {
    serde_json::to_value(table).map_err(|error| Error::usage(error.to_string()))
}

/// The Earnings Tick state: the newest released period without its statement items and segments,
/// the reaction summary and what comes next. Every period stays behind the history tool and one
/// period's statements and segments behind the detail tool.
fn summary(table: &EarningsTable) -> Result<serde_json::Value> {
    let mut latest = serde_json::to_value(table.rows.first())
        .map_err(|error| Error::usage(error.to_string()))?;
    if let Some(fields) = latest.as_object_mut() {
        fields.remove("statement_items");
        fields.remove("segments");
    }
    Ok(json!({
        "symbol": table.symbol, "source": table.source, "observed_at": table.observed_at,
        "availability": table.availability, "next_report": table.next_report,
        "upcoming": table.upcoming, "summary": table.summary, "latest": latest,
        "history": {"periods": table.rows.len(),
            "newest": table.rows.first().map(|row| &row.period_id),
            "oldest": table.rows.last().map(|row| &row.period_id), "tool": HISTORY_TOOL},
        "detail_tool": DETAIL_TOOL,
    }))
}

/// Duration until the next daily reload, anchored at [`DAILY_REFRESH_HOUR`] in the offset the
/// given instant carries. [`local_now`] supplies the machine's offset, so the Panel reloads on
/// the investor's calendar day rather than on a UTC one.
pub fn until_daily_refresh(now: time::OffsetDateTime) -> std::time::Duration {
    let anchor = |date: time::Date| {
        date.with_hms(DAILY_REFRESH_HOUR, 0, 0)
            .map(|at| at.assume_offset(now.offset()))
    };
    let next = anchor(now.date())
        .ok()
        .filter(|next| *next > now)
        .or_else(|| anchor(now.date() + time::Duration::days(1)).ok());
    next.and_then(|next| (next - now).try_into().ok())
        .unwrap_or(DAILY_REFRESH_FALLBACK)
}

/// Now in the machine's local offset. `time`'s own local-offset lookup refuses to run in a
/// multi-threaded process, so the offset comes from the C library's thread-safe conversion.
pub fn local_now() -> time::OffsetDateTime {
    let now = time::OffsetDateTime::now_utc();
    now.to_offset(local_offset(now.unix_timestamp()))
}

fn local_offset(unix_timestamp: i64) -> time::UtcOffset {
    let seconds = unix_timestamp as libc::time_t;
    let mut broken_down = unsafe { std::mem::zeroed::<libc::tm>() };
    // localtime_r writes into caller-owned storage and is the thread-safe conversion.
    if unsafe { libc::localtime_r(&seconds, &mut broken_down) }.is_null() {
        return time::UtcOffset::UTC;
    }
    i32::try_from(broken_down.tm_gmtoff)
        .ok()
        .and_then(|offset| time::UtcOffset::from_whole_seconds(offset).ok())
        .unwrap_or(time::UtcOffset::UTC)
}

struct SymbolPrompt {
    input: TextInput,
    error: Option<String>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum EarningsMode {
    Comparison,
    Detail,
    Reactions,
    Filings,
    Upcoming,
}

pub struct EarningsView {
    request: EarningsRequest,
    table: EarningsTable,
    prompt: Option<SymbolPrompt>,
    refresh_in: std::time::Duration,
    mode: EarningsMode,
    selected_period: usize,
    reset_scroll: std::cell::Cell<bool>,
    navigation: Option<ResearchNavigation>,
    pending_context: Option<ResearchContext>,
    filings_parent: EarningsMode,
}

impl EarningsView {
    pub fn new(request: EarningsRequest, table: EarningsTable) -> Self {
        let refresh_in = monitoring_interval(&table, today());
        Self {
            request,
            table,
            prompt: None,
            refresh_in,
            mode: EarningsMode::Comparison,
            selected_period: 0,
            reset_scroll: std::cell::Cell::new(false),
            navigation: None,
            pending_context: None,
            filings_parent: EarningsMode::Comparison,
        }
    }

    pub fn selected_detail(&self) -> Option<earnings::review::PeriodDetail> {
        earnings::review::detail(&self.table, self.selected_period)
    }

    fn current_context(&self) -> Option<ResearchContext> {
        if self.mode == EarningsMode::Upcoming {
            let upcoming = self.table.upcoming.as_ref()?;
            return Some(ResearchContext {
                symbol: self.table.symbol.clone(),
                period_id: upcoming.period_id.clone(),
                release_date: upcoming.release_date.clone(),
                release_session: event_session(upcoming.release_session),
                status: ResearchStatus::Upcoming,
                ..ResearchContext::default()
            });
        }
        let detail = self.selected_detail()?;
        Some(ResearchContext {
            symbol: detail.symbol,
            period_id: detail.period_id,
            fiscal_period_end: detail.fiscal_period_end,
            release_date: detail.report_date,
            release_session: event_session(detail.release_session),
            documents: detail
                .materials
                .into_iter()
                .map(|material| ResearchDocument {
                    title: material.title,
                    location: material.url,
                    kind: material.form,
                    published_at: material.published_at,
                    verification: material.verification.into(),
                    match_basis: material.match_basis.into(),
                })
                .collect(),
            ..ResearchContext::default()
        })
    }

    fn navigate(&mut self, target: ResearchTarget, selection: usize) {
        let Some(mut context) = self.current_context() else {
            return;
        };
        if self.mode == EarningsMode::Detail {
            if let Some(detail) = self.selected_detail() {
                if let Some(metric) = detail.metrics.get(selection) {
                    context.evidence = Some(ResearchEvidence {
                        label: metric.name.clone(),
                        text: format!(
                            "{} · {} · actual {:?}, estimate {:?}, units {} · source {} · observed {}; original passage not yet verified",
                            detail.period_id,
                            metric.name,
                            metric.actual,
                            metric.estimate,
                            metric.unit,
                            metric.source,
                            detail.observed_at.as_deref().unwrap_or("unknown")
                        ),
                        metric_key: Some(metric.field.clone()),
                        search_terms: metric_search_terms(&metric.field, &metric.name),
                        ..ResearchEvidence::default()
                    });
                } else if let Some(segment) = detail
                    .segments
                    .get(selection.saturating_sub(detail.metrics.len()))
                {
                    context.evidence = Some(ResearchEvidence {
                        label: segment.name.clone(),
                        text: format!(
                            "{} revenue {:?} {}",
                            segment.name, segment.revenue, segment.currency
                        ),
                        search_terms: vec![segment.name.clone()],
                        ..ResearchEvidence::default()
                    });
                }
            }
        } else if self.mode == EarningsMode::Filings && selection < context.documents.len() {
            context.documents = vec![context.documents[selection].clone()];
        }
        self.navigation = Some(ResearchNavigation { target, context });
    }

    async fn apply_prompt(&mut self, api: &QuoteServices) -> Result<()> {
        let Some(prompt) = &mut self.prompt else {
            return Ok(());
        };
        let request = match EarningsRequest::new(prompt.input.text(), Some(self.request.count)) {
            Ok(request) => request,
            Err(error) => {
                prompt.error = Some(error.message);
                return Ok(());
            }
        };
        let table = match load(api, &request, today()).await {
            Ok(table) => table,
            Err(error) => {
                if let Some(prompt) = &mut self.prompt {
                    prompt.error = Some(error.message);
                }
                return Ok(());
            }
        };
        self.prompt = None;
        self.refresh_in = monitoring_interval(&table, today());
        self.pending_context = None;
        self.table = table;
        self.request = request;
        self.selected_period = 0;
        self.mode = EarningsMode::Comparison;
        self.reset_scroll.set(true);
        Ok(())
    }

    fn draw_prompt(&self, frame: &mut Frame<'_>, settings: &Settings, prompt: &SymbolPrompt) {
        let area = frame.area();
        if area.height < 2 {
            return;
        }
        let styles = settings.styles();
        let line = Rect::new(area.x, area.bottom() - 2, area.width, 1);
        frame.render_widget(Clear, line);
        frame.buffer_mut().set_style(line, styles.overlay_base());
        let label = "Symbol: ";
        let label_width = tickloop_ui::format::width(label, settings.ambiguous_width);
        let available = (line.width as usize).saturating_sub(label_width);
        let (visible, cursor) = prompt.input.viewport(available, settings.ambiguous_width);
        let suffix = prompt
            .error
            .as_deref()
            .unwrap_or("Enter apply · Ctrl+C cancel");
        let text = format!("{label}{visible} · {suffix}");
        if available > 0 {
            frame.set_cursor_position((line.x + (label_width + cursor) as u16, line.y));
        }
        let style = if prompt.error.is_some() {
            styles.overlay_base().patch(styles.error)
        } else {
            styles.overlay_base()
        };
        write_text(
            frame.buffer_mut(),
            line.x,
            line.y,
            &text,
            line.width as usize,
            settings.ambiguous_width,
            style,
        );
    }
}

pub fn monitoring_interval(table: &EarningsTable, as_of: time::Date) -> std::time::Duration {
    let near_release = table
        .next_report
        .as_ref()
        .map(|event| event.date.as_str())
        .into_iter()
        .chain(
            table
                .rows
                .first()
                .and_then(|row| row.report_date.as_deref()),
        )
        .filter_map(tick_provider::dates::parse_iso_date)
        .any(|date| (date - as_of).whole_days().abs() <= 1);
    if near_release {
        std::time::Duration::from_secs(60)
    } else if table.upcoming.is_some() {
        std::time::Duration::from_secs(15 * 60)
    } else {
        until_daily_refresh(local_now())
    }
}

fn event_session(session: Option<&str>) -> crate::event_chart::EventSession {
    match session {
        Some("pre_market") => crate::event_chart::EventSession::PreMarket,
        Some("after_hours") => crate::event_chart::EventSession::AfterHours,
        _ => crate::event_chart::EventSession::Unknown,
    }
}

fn metric_search_terms(field: &str, name: &str) -> Vec<String> {
    let field = field.to_ascii_lowercase();
    let terms = if field.contains("eps") || field.contains("earningspershare") {
        vec!["diluted earnings per share", "earnings per share"]
    } else if field.contains("revenue") || field.contains("sales") {
        vec!["total net sales", "total revenue", "revenue"]
    } else if field.contains("totalassets") {
        vec!["total assets"]
    } else if field.contains("totalliabil") {
        vec!["total liabilities"]
    } else if field.contains("netincome") || field.contains("netprofit") {
        vec!["net income"]
    } else if field.contains("operating") && field.contains("cash") {
        vec![
            "cash generated by operating activities",
            "operating cash flow",
        ]
    } else {
        vec![name]
    };
    terms.into_iter().map(str::to_owned).collect()
}

#[async_trait::async_trait]
impl LiveView for EarningsView {
    fn research_context(&self) -> Option<ResearchContext> {
        self.current_context()
    }
    fn take_navigation(&mut self) -> Option<ResearchNavigation> {
        self.navigation.take()
    }
    fn tick(&self) -> crate::metadata::TickKind {
        crate::metadata::TickKind::Earnings
    }
    fn title(&self) -> String {
        let label = match self.mode {
            EarningsMode::Comparison => "Compare periods",
            EarningsMode::Detail => self
                .table
                .rows
                .get(self.selected_period)
                .map(|row| row.period_id.as_str())
                .unwrap_or("No period"),
            EarningsMode::Reactions => "Market reactions",
            EarningsMode::Filings => "Period originals · metadata matched",
            EarningsMode::Upcoming => "Upcoming earnings · preparing",
        };
        format!("{} · {label}", earnings::title(&self.table))
    }
    fn table(&self) -> TableData {
        match self.mode {
            EarningsMode::Comparison => earnings::review::comparison_table(&self.table),
            EarningsMode::Detail => self
                .selected_detail()
                .map(|detail| earnings::review::detail_table(&detail))
                .unwrap_or_else(|| TableData {
                    headers: vec!["Metric", "Actual"],
                    rows: Vec::new(),
                }),
            EarningsMode::Reactions => earnings::table(&self.table),
            EarningsMode::Filings => TableData {
                headers: vec!["Original", "Filed at", "Verification", "Source URL"],
                rows: self
                    .selected_detail()
                    .into_iter()
                    .flat_map(|detail| detail.materials)
                    .map(|material| {
                        vec![
                            json!(material.title),
                            json!(material.published_at),
                            json!(material.verification),
                            json!(material.url),
                        ]
                    })
                    .collect(),
            },
            EarningsMode::Upcoming => TableData {
                headers: vec!["Preparation", "Available value"],
                rows: self
                    .table
                    .upcoming
                    .as_ref()
                    .map(|period| {
                        vec![
                            vec![json!("Fiscal period"), json!(period.period_id)],
                            vec![json!("Status"), json!("Awaiting provider actuals")],
                            vec![
                                json!("Confirmed period release date"),
                                json!(period.release_date),
                            ],
                            vec![json!("Release session"), json!(period.release_session)],
                            vec![
                                json!("EPS estimate (queried now)"),
                                json!(period.eps_estimate),
                            ],
                            vec![
                                json!("Revenue estimate (queried now)"),
                                json!(period.revenue_estimate),
                            ],
                            vec![
                                json!("Historical pre-release snapshot"),
                                json!("Not available"),
                            ],
                            vec![
                                json!("Original"),
                                json!("Awaiting a period-matched issuer release"),
                            ],
                        ]
                    })
                    .unwrap_or_else(|| {
                        vec![vec![
                            json!("Upcoming fiscal period"),
                            json!("Provider has not identified an unreleased fiscal period"),
                        ]]
                    }),
            },
        }
    }
    fn content(&self) -> serde_json::Value {
        summary(&self.table).unwrap_or(serde_json::Value::Null)
    }
    fn priority(&self) -> &[usize] {
        match self.mode {
            EarningsMode::Comparison => earnings::review::COMPARISON_PRIORITY,
            EarningsMode::Detail => earnings::review::DETAIL_PRIORITY,
            EarningsMode::Reactions => earnings::COLUMN_PRIORITY,
            EarningsMode::Filings => &[0, 1, 2, 3],
            EarningsMode::Upcoming => &[0, 1],
        }
    }
    fn on_market(&mut self, _: &MarketEvent) {}
    fn refresh_interval(&self) -> Option<std::time::Duration> {
        Some(self.refresh_in)
    }
    fn refresh_on_recovery(&self) -> bool {
        true
    }
    async fn refresh(&mut self, api: &QuoteServices) -> Result<()> {
        self.refresh_in = monitoring_interval(&self.table, today());
        let request = self
            .pending_context
            .as_ref()
            .map(|context| EarningsRequest::new(&context.symbol, Some(MAX_PERIOD_COUNT)))
            .transpose()?
            .unwrap_or_else(|| self.request.clone());
        let selected_id = self.current_context().map(|context| context.period_id);
        let table = load(api, &request, today()).await?;
        if self.pending_context.is_none()
            && self.mode == EarningsMode::Upcoming
            && let Some(selected_id) = &selected_id
            && table
                .upcoming
                .as_ref()
                .is_none_or(|period| &period.period_id != selected_id)
            && find_period(&table, selected_id).is_err()
        {
            return Err(Error::network(
                "The selected upcoming fiscal period is missing from this provider response; the previous observation is retained",
            ));
        }
        if let Some(context) = &self.pending_context {
            if context.status == ResearchStatus::Upcoming
                && table
                    .upcoming
                    .as_ref()
                    .is_some_and(|period| period.period_id == context.period_id)
            {
                self.mode = EarningsMode::Upcoming;
            } else {
                self.selected_period = find_period(&table, &context.period_id)?;
                self.mode = EarningsMode::Detail;
            }
        }
        let had_pending = self.pending_context.take().is_some();
        self.table = table;
        self.request = request;
        if !had_pending
            && let Some(selected) = selected_id.and_then(|id| find_period(&self.table, &id).ok())
        {
            self.selected_period = selected;
            if self.mode == EarningsMode::Upcoming {
                self.mode = EarningsMode::Detail;
                self.reset_scroll.set(true);
            }
        }
        self.refresh_in = monitoring_interval(&self.table, today());
        Ok(())
    }
    fn captures_text(&self) -> bool {
        self.prompt.is_some()
    }
    fn cancel_text(&mut self) {
        self.prompt = None;
    }
    async fn on_key(
        &mut self,
        key: KeyCode,
        api: &QuoteServices,
        state: &mut ViewState,
    ) -> Result<bool> {
        if let Some(prompt) = &mut self.prompt {
            match prompt.input.handle(key) {
                EditOutcome::Submit => self.apply_prompt(api).await?,
                EditOutcome::Changed => prompt.error = None,
                EditOutcome::None => (),
            }
            return Ok(true);
        }
        match key {
            KeyCode::Char('R') => self.refresh(api).await?,
            KeyCode::Char('u') => {
                self.mode = EarningsMode::Upcoming;
                self.reset_scroll.set(true);
            }
            KeyCode::Char('o') => self.navigate(ResearchTarget::Reader, state.selected),
            KeyCode::Char('p') => self.navigate(ResearchTarget::Chart, state.selected),
            KeyCode::Char('v') => self.navigate(ResearchTarget::Options, state.selected),
            KeyCode::Char('a') => self.navigate(ResearchTarget::Chat, state.selected),
            KeyCode::Char('s') => {
                self.prompt = Some(SymbolPrompt {
                    input: TextInput::new(String::new()),
                    error: None,
                })
            }
            KeyCode::Enter
                if matches!(
                    self.mode,
                    EarningsMode::Comparison | EarningsMode::Reactions
                ) =>
            {
                if !self.table.rows.is_empty() {
                    self.mode = EarningsMode::Detail;
                    self.reset_scroll.set(true);
                }
            }
            KeyCode::Enter if matches!(self.mode, EarningsMode::Detail | EarningsMode::Filings) => {
                self.navigate(ResearchTarget::Reader, state.selected)
            }
            KeyCode::Char('c') => {
                self.mode = EarningsMode::Comparison;
                self.reset_scroll.set(true);
            }
            KeyCode::Backspace => {
                self.mode = if self.mode == EarningsMode::Filings {
                    self.filings_parent
                } else {
                    EarningsMode::Comparison
                };
                self.reset_scroll.set(true);
            }
            KeyCode::Char('r') => {
                self.mode = EarningsMode::Reactions;
                self.reset_scroll.set(true);
            }
            KeyCode::Char('f') => {
                if self.mode != EarningsMode::Filings {
                    self.filings_parent = self.mode;
                }
                self.mode = EarningsMode::Filings;
                self.reset_scroll.set(true);
            }
            KeyCode::Char('[') | KeyCode::Char(']') => {
                if self.mode == EarningsMode::Upcoming {
                    self.mode = EarningsMode::Detail;
                }
                if key == KeyCode::Char('[') {
                    self.selected_period =
                        (self.selected_period + 1).min(self.table.rows.len().saturating_sub(1));
                } else {
                    self.selected_period = self.selected_period.saturating_sub(1);
                }
                self.reset_scroll.set(true);
            }
            _ if matches!(
                self.mode,
                EarningsMode::Comparison | EarningsMode::Reactions
            ) =>
            {
                let last = self.table.rows.len().saturating_sub(1);
                self.selected_period = match key {
                    KeyCode::Char('j') | KeyCode::Down => (self.selected_period + 1).min(last),
                    KeyCode::Char('k') | KeyCode::Up => self.selected_period.saturating_sub(1),
                    KeyCode::Char('g') | KeyCode::Home => 0,
                    KeyCode::Char('G') | KeyCode::End => last,
                    KeyCode::PageDown => (self.selected_period + 10).min(last),
                    KeyCode::PageUp => self.selected_period.saturating_sub(10),
                    _ => return Ok(false),
                };
                state.selected = self.selected_period;
            }
            _ => return Ok(false),
        }
        Ok(true)
    }
    async fn act(&mut self, call: ToolCall) -> Result<Value> {
        if call.tool == "refresh" {
            let _: EmptyArgs = call.arguments()?;
            self.refresh_in = std::time::Duration::from_millis(1);
            return Ok(json!({"refresh":"scheduled"}));
        }
        if call.tool == "set_context" {
            let context = call.arguments::<SetResearchContext>()?.context;
            context.validate()?;
            if context.symbol == self.table.symbol {
                if context.status == ResearchStatus::Upcoming
                    && self
                        .table
                        .upcoming
                        .as_ref()
                        .is_some_and(|period| period.period_id == context.period_id)
                {
                    self.mode = EarningsMode::Upcoming;
                    self.reset_scroll.set(true);
                    return Ok(json!({"context":self.current_context()}));
                }
                if let Ok(selected) = find_period(&self.table, &context.period_id) {
                    self.selected_period = selected;
                    self.mode = EarningsMode::Detail;
                    self.reset_scroll.set(true);
                    return Ok(json!({"context":self.current_context()}));
                }
            }
            EarningsRequest::new(&context.symbol, Some(MAX_PERIOD_COUNT))?;
            self.pending_context = Some(context);
            self.refresh_in = std::time::Duration::from_millis(1);
            return Ok(json!({"context_load":"scheduled"}));
        }
        if call.tool != "select_period" {
            return Err(Error::usage(format!(
                "Earnings offers no action Tick Tool {:?}",
                call.tool
            )));
        }
        let args: SelectPeriodArgs = call.arguments()?;
        self.selected_period = find_period(&self.table, &args.period_id)?;
        self.mode = EarningsMode::Detail;
        self.reset_scroll.set(true);
        serde_json::to_value(self.selected_detail())
            .map_err(|error| Error::usage(error.to_string()))
    }
    fn help(&self) -> &'static str {
        "q / Ctrl+C  Quit\n?  Help · Backspace returns one level\nj / k / Ctrl+N / Ctrl+P / arrows  Move\nEnter  Period detail / selected metric original · c  Compare\n[ / ]  Older / newer fiscal period · u  Upcoming\no  Original · p  Chart · v  Options · a  Ask Chat\nr  Market reactions · f  Period originals · R  Refresh\ns  Change symbol (Enter apply · Ctrl+C cancel)\nPageUp / PageDown  Page · g / G  First / last\nEstimates retrieved now; no verified historical snapshot\nUS symbols · 60s near release, 15m upcoming, daily history"
    }
    fn draw(&self, frame: &mut Frame<'_>, state: &mut ViewState, settings: &Settings) {
        if self.reset_scroll.replace(false) {
            state.selected = 0;
            state.offset = 0;
        }
        if matches!(
            self.mode,
            EarningsMode::Comparison | EarningsMode::Reactions
        ) {
            state.selected = self.selected_period;
        }
        crate::runtime::draw_table(
            frame,
            &self.title(),
            &self.table(),
            self.priority(),
            state,
            settings,
        );
        let area = frame.area();
        if area.height >= 10 {
            let styles = settings.styles();
            let observed = self.table.observed_at.as_deref().unwrap_or("unknown");
            let source = if self.table.source == "demo" {
                "Demo"
            } else {
                "Longbridge"
            };
            let (facts, hints): (Vec<String>, &[Hint]) = match self.mode {
                EarningsMode::Detail => {
                    let missing = self
                        .table
                        .availability
                        .iter()
                        .filter(|item| !item.available)
                        .map(|item| item.dataset)
                        .collect::<Vec<_>>()
                        .join(", ");
                    let mut facts = vec![
                        source.to_owned(),
                        observed.to_owned(),
                        "estimates not archived".to_owned(),
                    ];
                    if !missing.is_empty() {
                        facts.push(format!("unavailable: {missing}"));
                    }
                    (facts, &[])
                }
                EarningsMode::Filings => (
                    vec![
                        source.to_owned(),
                        "filename metadata matched; body not verified".to_owned(),
                    ],
                    &[Hint::whole("Enter opens original")],
                ),
                EarningsMode::Upcoming => (
                    vec![
                        source.to_owned(),
                        observed.to_owned(),
                        "awaiting actuals".to_owned(),
                        format!("refresh {}s", self.refresh_in.as_secs()),
                    ],
                    &[Hint::whole("R refresh")],
                ),
                _ => (
                    vec![source.to_owned(), observed.to_owned()],
                    &[
                        Hint::whole("Enter detail"),
                        Hint::whole("c compare"),
                        Hint::whole("r reactions"),
                        Hint::whole("[ ] periods"),
                    ],
                ),
            };
            let content = crate::runtime::table_content_area(area);
            let columns = content.width as usize;
            write_text(
                frame.buffer_mut(),
                content.x,
                area.y + 1,
                &info_line(&facts, hints, columns, settings.ambiguous_width),
                columns,
                settings.ambiguous_width,
                styles.panel_base().patch(styles.muted_text),
            );
        }
        if let Some(prompt) = &self.prompt {
            self.draw_prompt(frame, settings, prompt);
        }
    }
}

/// Joins the facts, truncating them only when they alone overflow, then the key hints that
/// still fit whole; the help overlay lists every key the line drops.
fn info_line(
    facts: &[String],
    hints: &[Hint<'_>],
    columns: usize,
    ambiguous: AmbiguousWidth,
) -> String {
    let facts = truncate(&facts.join(INFO_SEPARATOR), columns, ambiguous);
    let room = columns.saturating_sub(width(&facts, ambiguous) + width(INFO_SEPARATOR, ambiguous));
    let keys = fit_hints(hints, INFO_SEPARATOR, room, ambiguous);
    if keys.is_empty() {
        facts
    } else {
        format!("{facts}{INFO_SEPARATOR}{keys}")
    }
}

pub async fn run_live<
    B: ratatui::backend::Backend,
    S: futures_util::Stream<Item = std::io::Result<crossterm::event::Event>> + Unpin,
>(
    args: &EarningsArgs,
    api: &QuoteServices,
    settings: &Settings,
    terminal: &mut ratatui::Terminal<B>,
    mut input: S,
    updates: &mut TickStream,
) -> Result<()> {
    let request = request(args)?;
    let Some(table) = crate::runtime::cancellable(load(api, &request, today()), &mut input).await?
    else {
        return Ok(());
    };
    let mut view = EarningsView::new(request, table);
    if let Some(context) = ResearchContext::from_environment()? {
        view.act(ToolCall {
            tool: "set_context".into(),
            args: json!({"context":context}),
        })
        .await?;
    }
    let (_market_sender, market) = tokio::sync::mpsc::unbounded_channel();
    crate::runtime::run(&mut view, api, settings, terminal, input, market, updates).await
}

#[cfg(test)]
mod info_line_tests {
    use super::info_line;
    use tickloop_ui::{format::Hint, settings::AmbiguousWidth};

    #[test]
    fn key_hints_drop_whole_while_facts_truncate_only_when_they_alone_overflow() {
        let facts = ["Demo".to_owned(), "2026-10-10T06:00:00Z".to_owned()];
        let hints = [Hint::whole("Enter detail"), Hint::whole("c compare")];
        let line = |columns| info_line(&facts, &hints, columns, AmbiguousWidth::Narrow);

        assert_eq!(
            line(80),
            "Demo · 2026-10-10T06:00:00Z · Enter detail · c compare"
        );
        assert_eq!(line(50), "Demo · 2026-10-10T06:00:00Z · Enter detail");
        assert_eq!(line(30), "Demo · 2026-10-10T06:00:00Z");
        assert_eq!(line(12), "Demo · 2026…");
    }
}
