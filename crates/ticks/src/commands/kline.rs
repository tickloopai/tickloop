use std::io::Write;

use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::{
    api::{
        Candle, CandleAdjust, CandleHistoryRequest, CandlePeriod, CandleRequest, DateRange,
        QuoteServices, normalize_symbol,
    },
    cli::KlineArgs,
    config::Settings,
    error::{Error, Result},
    event_chart::{
        EventChart, EventChartConfig, EventChartMode, EventEditResult, EventEditor, EventRange,
    },
    output::TableData,
    research::{
        ResearchContext, ResearchEvidence, ResearchNavigation, ResearchTarget, SetResearchContext,
    },
    stream::TickStream,
    tools::{ToolCall, Tools},
};

pub const MAX_CANDLES: usize = 1000;
pub const DEFAULT_ONCE_COUNT: usize = 80;
/// The widest date range one `candles` call may ask the market for, so that a
/// Tick Tool call cannot turn into an unbounded history request.
pub const MAX_HISTORY_DAYS: i64 = 1000;
const MARK_TOOL: &str = "mark";
const UNMARK_TOOL: &str = "unmark";
pub const HEADERS: &[&str] = &[
    "Timestamp",
    "Open",
    "High",
    "Low",
    "Close",
    "Volume",
    "Turnover",
];

pub fn table(candles: &[Candle]) -> TableData {
    TableData {
        headers: HEADERS.to_vec(),
        rows: candles
            .iter()
            .map(|candle| {
                vec![
                    json!(
                        candle
                            .timestamp
                            .format(&time::format_description::well_known::Rfc3339)
                            .unwrap_or_default()
                    ),
                    json!(candle.open.normalize().to_string()),
                    json!(candle.high.normalize().to_string()),
                    json!(candle.low.normalize().to_string()),
                    json!(candle.close.normalize().to_string()),
                    json!(candle.volume),
                    json!(candle.turnover.normalize().to_string()),
                ]
            })
            .collect(),
    }
}

/// A labelled price line the Agent drew on the running Panel. Marks live only
/// in this Tick Instance: a replaced or restarted Kline starts without them and
/// the Agent re-draws them from its own transcript.
#[derive(Clone, Debug, PartialEq, serde::Serialize)]
pub struct PriceMark {
    pub label: String,
    pub price: f64,
}

fn content(request: &CandleRequest, candles: &[Candle], marks: &[PriceMark]) -> Value {
    json!({"symbol":request.symbol, "period":request.period, "adjust":request.adjust,
        "update_source":match request.adjust { CandleAdjust::None => "server_snapshot", CandleAdjust::Forward => "server_adjusted_snapshot" },
        "candles":table(candles).json(), "marks":marks})
}

/// Arguments of the `candles` Tick Tool; the schema the Agent reads is derived from it.
#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CandlesArgs {
    /// Symbol to load, such as `AAPL.US` or `700.HK`.
    pub symbol: String,
    /// Candlestick period; daily candles when omitted.
    #[serde(default)]
    pub period: CandlePeriod,
    /// Price adjustment; unadjusted prices when omitted.
    #[serde(default)]
    pub adjust: CandleAdjust,
    /// Inclusive first date, as `YYYY-MM-DD`.
    pub from: String,
    /// Inclusive last date, as `YYYY-MM-DD`.
    pub to: String,
}

impl CandlesArgs {
    fn history(&self) -> Result<CandleHistoryRequest> {
        let start = parse_day("from", &self.from)?;
        let end = parse_day("to", &self.to)?;
        if start > end {
            return Err(Error::usage("from must not be later than to"));
        }
        if (end - start).whole_days() >= MAX_HISTORY_DAYS {
            return Err(Error::usage(format!(
                "candles covers at most {MAX_HISTORY_DAYS} days per call"
            )));
        }
        // The day span alone lets a minute period ask for hundreds of thousands of candles; the
        // answer is capped at the same count the Panel's own request is.
        let candles = self
            .period
            .most_candles_in((end - start).whole_days().unsigned_abs() + 1);
        if candles > MAX_CANDLES as u64 {
            return Err(Error::usage(format!(
                "candles returns at most {MAX_CANDLES} candles per call; {} over this range is about {candles}",
                self.period.name()
            )));
        }
        Ok(CandleHistoryRequest {
            symbol: normalize_symbol(&self.symbol),
            period: self.period,
            adjust: self.adjust,
            range: DateRange { start, end },
        })
    }
}

fn parse_day(field: &str, text: &str) -> Result<time::Date> {
    crate::dates::parse_iso_date(text)
        .ok_or_else(|| Error::usage(format!("{field} must be a date in YYYY-MM-DD form")))
}

/// Arguments of the `mark` action; re-using a label moves that line.
#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct MarkArgs {
    /// Price level the line is drawn at.
    pub price: f64,
    /// Short label drawn beside the line.
    pub label: String,
}

/// Arguments of the `unmark` action.
#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct UnmarkArgs {
    /// Label of the line to remove.
    pub label: String,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct EventHistoryArgs {
    pub symbol: String,
    #[serde(default)]
    pub adjust: CandleAdjust,
    pub event: EventChartConfig,
    #[serde(default)]
    pub offset: usize,
    pub limit: Option<usize>,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SetEventArgs {
    pub event: EventChartConfig,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct EventViewArgs {
    pub view: EventChartMode,
    pub range: Option<EventRange>,
}

pub fn tools() -> Tools {
    Tools::default()
        .query::<CandlesArgs>(
            "candles",
            "Candlesticks for a symbol between two dates, in the Kline Panel's shape.",
            |api: &QuoteServices, args| {
                Box::pin(async move {
                    let request = args.history()?;
                    let candles = api.candles_by_date(&request).await?;
                    Ok(json!({"symbol": request.symbol, "period": request.period,
                        "adjust": request.adjust, "candles": table(&candles).json()}))
                })
            },
        )
        .action::<MarkArgs>(
            MARK_TOOL,
            "Draw a labelled horizontal price line on this Panel.",
        )
        .action::<UnmarkArgs>(UNMARK_TOOL, "Remove a labelled price line from this Panel.")
        .query::<EventHistoryArgs>(
            "event_history",
            "Event-aware history, provider calendar, sessions and pre-release baseline; at most 1000 candles per page. Follow next_offset to recover the full window. An omitted actual publication time never becomes a guessed T=0.",
            |api: &QuoteServices, args| Box::pin(async move {
                let symbol = normalize_symbol(&args.symbol);
                let limit = args.limit.unwrap_or(crate::event_chart::MAX_EVENT_PAGE_CANDLES);
                if !(1..=crate::event_chart::MAX_EVENT_PAGE_CANDLES).contains(&limit) {
                    return Err(Error::usage("event_history limit must be between 1 and 1000"));
                }
                let data = crate::event_chart::load_history(api, &symbol, args.adjust, &args.event).await?;
                if args.offset > data.candles.len() { return Err(Error::usage("event_history offset is beyond the window")); }
                Ok(crate::event_chart::history_page(&symbol, args.adjust, &args.event, &data, args.offset, limit))
            }),
        )
        .action::<SetEventArgs>("set_event", "Set the publication event. Clears the old event chart and schedules history loading; status reports loading, ready or unavailable.")
        .action::<SetResearchContext>("set_context", "Follow this company and fiscal period in the same instance. Uses the supplied release date/session without inventing an exact publication time; clears previous event data and marks.")
        .action::<EventViewArgs>("set_view", "Select event line, 5-minute candles, or daily candles and optionally an event window. Reloads the requested data.")
        .recover_with("candles")
}

fn request(args: &KlineArgs, default_count: usize) -> Result<CandleRequest> {
    let count = args.count.unwrap_or(default_count);
    if !(1..=MAX_CANDLES).contains(&count) {
        return Err(Error::usage(format!(
            "count must be between 1 and {MAX_CANDLES}"
        )));
    }
    Ok(CandleRequest {
        symbol: normalize_symbol(&args.symbol),
        period: args.period,
        count,
        adjust: args.adjust,
    })
}

pub async fn run_once(
    args: &KlineArgs,
    api: &QuoteServices,
    settings: &Settings,
    color: bool,
    output: &mut impl Write,
    updates: &mut TickStream,
) -> Result<()> {
    let request = request(args, DEFAULT_ONCE_COUNT)?;
    if let Some(config) = args.event.config()? {
        let data =
            crate::event_chart::load_history(api, &request.symbol, request.adjust, &config).await?;
        let content =
            crate::event_chart::history_content(&request.symbol, request.adjust, &config, &data);
        updates.publish(content.clone())?;
        if matches!(args.output.format, crate::cli::OutputFormat::Json) {
            serde_json::to_writer_pretty(&mut *output, &content)
                .map_err(|error| Error::usage(error.to_string()))?;
            writeln!(output).map_err(|error| Error::usage(error.to_string()))?;
            return Ok(());
        }
        return table(&data.candles).write(args.output.format, settings, color, output);
    }
    let candles = api.candles(&request).await?;
    updates.publish(content(&request, &candles, &[]))?;
    table(&candles).write(args.output.format, settings, color, output)
}

const POLL_INTERVAL: std::time::Duration = std::time::Duration::from_secs(30);
const MOVING_AVERAGE_WINDOWS: [usize; 3] = [5, 10, 20];

pub struct KlineView {
    pub request: CandleRequest,
    pub candles: Vec<Candle>,
    pub show_averages: bool,
    marks: Vec<PriceMark>,
    cursor: usize,
    viewport_start: std::cell::Cell<usize>,
    visible_count: std::cell::Cell<usize>,
    supports_push: bool,
    last_snapshot_request: Option<tokio::time::Instant>,
    subscriptions: std::collections::BTreeSet<crate::api::CandlePeriod>,
    event: Option<EventChart>,
    event_editor: Option<EventEditor>,
    research: Option<ResearchContext>,
    navigation: Option<ResearchNavigation>,
    retired_subscriptions: Vec<(String, CandlePeriod)>,
}

impl KlineView {
    pub fn new(request: CandleRequest, mut candles: Vec<Candle>, supports_push: bool) -> Self {
        candles.sort_by_key(|candle| candle.timestamp);
        candles.dedup_by_key(|candle| candle.timestamp);
        if candles.len() > request.count {
            candles.drain(..candles.len() - request.count);
        }
        Self {
            request,
            cursor: candles.len().saturating_sub(1),
            candles,
            show_averages: false,
            marks: Vec::new(),
            viewport_start: std::cell::Cell::new(usize::MAX),
            visible_count: std::cell::Cell::new(1),
            supports_push,
            last_snapshot_request: None,
            subscriptions: std::collections::BTreeSet::new(),
            event: None,
            event_editor: None,
            research: None,
            navigation: None,
            retired_subscriptions: Vec::new(),
        }
    }

    pub fn with_event(request: CandleRequest, config: EventChartConfig) -> Result<Self> {
        let mut view = Self::new(request, Vec::new(), false);
        view.set_event(config)?;
        Ok(view)
    }

    pub fn set_event(&mut self, config: EventChartConfig) -> Result<()> {
        self.event = Some(EventChart::new(config)?);
        self.supports_push = false;
        self.research = None;
        Ok(())
    }

    pub fn set_research_context(&mut self, context: ResearchContext) -> Result<()> {
        context.validate()?;
        let symbol = normalize_symbol(&context.symbol);
        let event = context
            .release_date
            .as_ref()
            .map(|date| EventChartConfig {
                date: date.clone(),
                session: context.release_session,
                published_at: context.published_at.clone(),
                conference_at: context.conference_at.clone(),
                view: self
                    .event
                    .as_ref()
                    .map_or(EventChartMode::Line, |event| event.config.view),
                range: self
                    .event
                    .as_ref()
                    .map_or(EventRange::Auto, |event| event.config.range),
            })
            .map(EventChart::new)
            .transpose()?;
        self.retired_subscriptions.extend(
            std::mem::take(&mut self.subscriptions)
                .into_iter()
                .map(|period| (self.request.symbol.clone(), period)),
        );
        self.request.symbol = symbol;
        self.candles.clear();
        self.marks.clear();
        self.supports_push = false;
        self.event = event;
        self.event_editor = None;
        self.research = Some(context);
        self.navigation = None;
        Ok(())
    }

    pub async fn load_event(&mut self, api: &QuoteServices) -> Result<()> {
        while let Some((symbol, period)) = self.retired_subscriptions.last() {
            api.unsubscribe_candles(symbol, *period).await?;
            self.retired_subscriptions.pop();
        }
        if let Some(event) = &mut self.event {
            event.pending = false;
        }
        let subscriptions: Vec<_> = self.subscriptions.iter().copied().collect();
        for period in subscriptions {
            self.remove_subscription(api, period).await?;
        }
        if let Some(event) = &mut self.event {
            event
                .reload(api, &self.request.symbol, self.request.adjust)
                .await?;
        }
        Ok(())
    }

    fn replace_candles(&mut self, candles: Vec<Candle>) {
        let followed_last = self.cursor == self.candles.len().saturating_sub(1);
        let selected_time = self.candles.get(self.cursor).map(|candle| candle.timestamp);
        let fresh = Self::new(self.request.clone(), candles, self.supports_push);
        self.candles = fresh.candles;
        self.cursor = if followed_last {
            self.candles.len().saturating_sub(1)
        } else {
            self.candles
                .iter()
                .position(|candle| Some(candle.timestamp) == selected_time)
                .unwrap_or(0)
        };
    }

    fn draw_mark(&mut self, args: MarkArgs) -> Result<()> {
        if !args.price.is_finite() {
            return Err(Error::usage("price must be a finite number"));
        }
        let label = args.label.trim();
        if label.is_empty() {
            return Err(Error::usage("label must not be empty"));
        }
        match self.marks.iter_mut().find(|mark| mark.label == label) {
            Some(existing) => existing.price = args.price,
            None => self.marks.push(PriceMark {
                label: label.to_owned(),
                price: args.price,
            }),
        }
        Ok(())
    }

    fn erase_mark(&mut self, label: &str) -> Result<()> {
        let label = label.trim();
        let before = self.marks.len();
        self.marks.retain(|mark| mark.label != label);
        if self.marks.len() == before {
            return Err(Error::usage(format!(
                "this Panel has no mark labelled {label:?}"
            )));
        }
        Ok(())
    }

    async fn remove_subscription(
        &mut self,
        api: &QuoteServices,
        period: crate::api::CandlePeriod,
    ) -> Result<()> {
        tokio::time::timeout(
            crate::runtime::CLEANUP_TIMEOUT,
            api.unsubscribe_candles(&self.request.symbol, period),
        )
        .await
        .unwrap_or_else(|_| Err(Error::network("subscription cleanup timed out")))?;
        self.subscriptions.remove(&period);
        Ok(())
    }

    async fn rollback_period_switch(
        &mut self,
        api: &QuoteServices,
        new_period: crate::api::CandlePeriod,
        restore_previous: bool,
    ) -> Result<()> {
        let restored = if restore_previous {
            let result = tokio::time::timeout(
                crate::runtime::CLEANUP_TIMEOUT,
                api.subscribe_candles(&self.request),
            )
            .await
            .unwrap_or_else(|_| Err(Error::network("subscription restoration timed out")));
            self.supports_push = matches!(result, Ok(true));
            if matches!(result, Ok(false)) {
                self.subscriptions.remove(&self.request.period);
            }
            result.map(|_| ())
        } else {
            Ok(())
        };
        let removed = self.remove_subscription(api, new_period).await;
        restored.and(removed)
    }
}

#[async_trait::async_trait]
impl crate::runtime::LiveView for KlineView {
    fn tick(&self) -> crate::metadata::TickKind {
        crate::metadata::TickKind::Kline
    }
    fn title(&self) -> String {
        if let Some(context) = &self.research {
            return format!("{} · Event chart", context.title());
        }
        if let Some(event) = &self.event {
            return format!(
                "{} · {} · {}",
                self.request.symbol,
                event.config.date,
                event.config.view.label()
            );
        }
        format!(
            "{} · {} · {}",
            self.request.symbol,
            self.request.period.name(),
            self.request.adjust.name()
        )
    }
    fn table(&self) -> TableData {
        if let Some(event) = &self.event {
            return table(
                event
                    .data
                    .as_ref()
                    .map_or(&[], |data| data.candles.as_slice()),
            );
        }
        table(&self.candles)
    }
    fn content(&self) -> Value {
        if let Some(event) = &self.event {
            let mut content = event.content(&self.request.symbol, self.request.adjust);
            content["marks"] = json!(self.marks);
            content["research_context"] = json!(self.research);
            return content;
        }
        if let Some(context) = &self.research {
            return json!({"symbol":self.request.symbol,"research_context":context,"status":"unavailable","error":"Release date is unknown; event window cannot be chosen","candles":[]});
        }
        content(&self.request, &self.candles, &self.marks)
    }
    fn research_context(&self) -> Option<ResearchContext> {
        self.research.clone()
    }
    fn take_navigation(&mut self) -> Option<ResearchNavigation> {
        self.navigation.take()
    }
    fn priority(&self) -> &[usize] {
        &[0, 4, 1, 2, 3, 5, 6]
    }
    fn refresh_interval(&self) -> Option<std::time::Duration> {
        if self.event.as_ref().is_some_and(|event| event.pending) {
            return Some(std::time::Duration::from_millis(1));
        }
        Some(POLL_INTERVAL)
    }
    fn refresh_on_recovery(&self) -> bool {
        true
    }
    async fn refresh(&mut self, api: &QuoteServices) -> Result<()> {
        if self.event.is_some() || self.research.is_some() {
            return self.load_event(api).await;
        }
        let requested_at = tokio::time::Instant::now();
        self.replace_candles(api.candles(&self.request).await?);
        self.last_snapshot_request = Some(requested_at);
        Ok(())
    }
    async fn act(&mut self, call: ToolCall) -> Result<Value> {
        match call.tool.as_str() {
            "set_context" => {
                self.set_research_context(call.arguments::<SetResearchContext>()?.context)?;
                return Ok(self.content());
            }
            "set_event" => {
                self.set_event(call.arguments::<SetEventArgs>()?.event)?;
                return Ok(self.content());
            }
            "set_view" => {
                let args: EventViewArgs = call.arguments()?;
                let event = self
                    .event
                    .as_mut()
                    .ok_or_else(|| Error::usage("set an event before choosing its view"))?;
                event.set_view(args.view, args.range.unwrap_or(event.config.range));
                return Ok(self.content());
            }
            MARK_TOOL => self.draw_mark(call.arguments()?)?,
            UNMARK_TOOL => self.erase_mark(&call.arguments::<UnmarkArgs>()?.label)?,
            unknown => {
                return Err(Error::usage(format!(
                    "Kline offers no action Tick Tool {unknown:?}"
                )));
            }
        }
        Ok(json!({"marks": self.marks}))
    }
    fn help(&self) -> &'static str {
        "q / Ctrl+C  Quit · Esc Back\nh/l or Left/Right  Select candle\nj/k or Ctrl+N/P  Navigate visible list / event samples\ne  Edit publication event (Tab field, Enter apply)\nv  Event view picker · r Event range picker\n[ / ]  Previous / next event range\nEnter  Event candle detail · Esc Back\na  Ask Chat about the selected research observation\ng / G  First / latest candle\nPageUp / PageDown  Move one screen\nOrdinary Kline: 1 day · 2 week · 3 month · 4 1m\n5 5m · 6 15m · 7 30m · 8 60m · m MA\nHistory refresh every 30s; API calendar determines sessions"
    }
    fn captures_text(&self) -> bool {
        self.event_editor.is_some()
    }
    fn on_market(&mut self, event: &crate::api::MarketEvent) {
        if self.event.is_some() || self.research.is_some() {
            return;
        }
        if let crate::api::MarketEvent::Candles(update) = event {
            if update.symbol != self.request.symbol
                || update.period != self.request.period
                || update.adjust != self.request.adjust
            {
                return;
            }
            if update.replace {
                if let (Some(current), Some(incoming)) =
                    (self.last_snapshot_request, update.requested_at)
                    && incoming < current
                {
                    return;
                }
                if let (Some(current), Some(incoming)) =
                    (self.candles.last(), update.candles.last())
                    && incoming.timestamp < current.timestamp
                {
                    return;
                }
                self.replace_candles(update.candles.clone());
                if update.requested_at.is_some() {
                    self.last_snapshot_request = update.requested_at;
                }
            } else {
                if self.request.adjust == CandleAdjust::Forward {
                    return;
                }
                let mut candles = self.candles.clone();
                for candle in &update.candles {
                    match candles.last_mut() {
                        Some(last) if candle.timestamp == last.timestamp => {
                            // A queued SDK aggregate can predate an authoritative history sync.
                            if candle.volume >= last.volume
                                && candle.turnover >= last.turnover
                                && candle.high >= last.high
                                && candle.low <= last.low
                                && candle.open == last.open
                            {
                                *last = candle.clone();
                            }
                        }
                        Some(last) if candle.timestamp < last.timestamp => (),
                        _ => candles.push(candle.clone()),
                    }
                }
                self.replace_candles(candles);
            }
        }
    }
    async fn on_key(
        &mut self,
        key: crossterm::event::KeyCode,
        api: &QuoteServices,
        _state: &mut crate::runtime::ViewState,
    ) -> Result<bool> {
        use crossterm::event::KeyCode;
        if let Some(editor) = &mut self.event_editor {
            match editor.on_key(key) {
                EventEditResult::Continue => (),
                EventEditResult::Cancel => self.event_editor = None,
                EventEditResult::Apply(config) => {
                    self.set_event(config)?;
                    self.event_editor = None;
                    self.load_event(api).await?;
                }
            }
            return Ok(true);
        }
        if let Some(event) = &mut self.event
            && event.has_overlay()
        {
            return event
                .on_key(key, api, &self.request.symbol, self.request.adjust)
                .await;
        }
        if key == KeyCode::Char('a')
            && let Some(context) = &self.research
        {
            let mut context = context.clone();
            context.evidence = Some(ResearchEvidence {
                label: "Selected event-chart observation".into(),
                text: self.event.as_ref().map_or_else(
                    || "Release date unknown; chart unavailable".into(),
                    |event| event.selected_observation().to_string(),
                ),
                origin: self
                    .event
                    .as_ref()
                    .and_then(|event| event.data.as_ref())
                    .map(|data| data.source.into()),
                ..Default::default()
            });
            self.navigation = Some(ResearchNavigation {
                target: ResearchTarget::Chat,
                context,
            });
            return Ok(true);
        }
        if key == KeyCode::Char('e') {
            self.event_editor = Some(EventEditor::new(
                &self.request.symbol,
                self.event.as_ref().map(|event| &event.config),
            )?);
            return Ok(true);
        }
        if let Some(event) = &mut self.event {
            return event
                .on_key(key, api, &self.request.symbol, self.request.adjust)
                .await;
        }
        match key {
            KeyCode::Esc => (),
            KeyCode::Left | KeyCode::Char('h') | KeyCode::Up | KeyCode::Char('k') => {
                self.cursor = self.cursor.saturating_sub(1)
            }
            KeyCode::Right | KeyCode::Char('l') | KeyCode::Down | KeyCode::Char('j') => {
                self.cursor = (self.cursor + 1).min(self.candles.len().saturating_sub(1))
            }
            KeyCode::Char('g') | KeyCode::Home => self.cursor = 0,
            KeyCode::Char('G') | KeyCode::End => self.cursor = self.candles.len().saturating_sub(1),
            KeyCode::PageUp => self.cursor = self.cursor.saturating_sub(self.visible_count.get()),
            KeyCode::PageDown => {
                self.cursor = (self.cursor + self.visible_count.get())
                    .min(self.candles.len().saturating_sub(1))
            }
            KeyCode::Char('m') => self.show_averages = !self.show_averages,
            KeyCode::Char(number @ '1'..='8') => {
                let period = crate::api::CandlePeriod::ALL[(number as u8 - b'1') as usize];
                if period == self.request.period {
                    return Ok(true);
                }
                let next_request = CandleRequest {
                    period,
                    ..self.request.clone()
                };
                let requested_at = tokio::time::Instant::now();
                let candles = api.candles(&next_request).await?;
                self.subscriptions.insert(period);
                let mut retired_previous = false;
                let switched = async {
                    let supports_push = api.subscribe_candles(&next_request).await?;
                    if !supports_push {
                        self.subscriptions.remove(&period);
                    }
                    if self.subscriptions.contains(&self.request.period) {
                        retired_previous = true;
                        self.remove_subscription(api, self.request.period).await?;
                    }
                    Ok(supports_push)
                }
                .await;
                let supports_push = match switched {
                    Ok(supports_push) => supports_push,
                    Err(error) => {
                        let rollback = self
                            .rollback_period_switch(api, period, retired_previous)
                            .await;
                        return match rollback {
                            Err(rollback) if rollback.code == crate::exit_code::NOT_AUTHORIZED => {
                                Err(rollback)
                            }
                            _ => Err(error),
                        };
                    }
                };
                self.request = next_request;
                self.supports_push = supports_push;
                self.replace_candles(candles);
                self.last_snapshot_request = Some(requested_at);
                self.cursor = self.candles.len().saturating_sub(1);
                self.viewport_start.set(usize::MAX);
            }
            _ => return Ok(false),
        }
        Ok(true)
    }
    fn draw(
        &self,
        frame: &mut ratatui::Frame<'_>,
        _state: &mut crate::runtime::ViewState,
        settings: &Settings,
    ) {
        if self.research.is_some() && self.event.is_none() {
            frame.render_widget(ratatui::widgets::Paragraph::new(format!("{}\nRelease date is unknown. No event window can be chosen.\na Ask Chat · e Edit event", self.title())).style(settings.styles().text), frame.area());
            if let Some(editor) = &self.event_editor {
                editor.draw(frame, settings);
            }
            return;
        }
        if let Some(event) = &self.event {
            event.draw(frame, &self.request.symbol, settings, &self.marks);
            if let Some(context) = &self.research
                && !event.has_overlay()
                && self.event_editor.is_none()
                && frame.area().height >= 14
            {
                let area = frame.area();
                tickloop_ui::format::write_text(
                    frame.buffer_mut(),
                    area.x + 2,
                    area.y,
                    &format!(
                        " {} · {} · {} ",
                        context.title(),
                        event.config.date,
                        event.config.view.label()
                    ),
                    usize::from(area.width.saturating_sub(4)),
                    settings.ambiguous_width,
                    settings.styles().title,
                );
                tickloop_ui::format::write_text(
                    frame.buffer_mut(),
                    area.x + 1,
                    area.bottom() - 2,
                    "a Ask Chat · ? Help",
                    usize::from(area.width.saturating_sub(2)),
                    settings.ambiguous_width,
                    settings.styles().status_base(),
                );
            }
            if let Some(editor) = &self.event_editor {
                editor.draw(frame, settings);
            }
            return;
        }
        use ratatui::widgets::{Block, Borders, Paragraph};
        use rust_decimal::prelude::ToPrimitive;
        use tickloop_ui::{
            format::{width, write_text},
            styles::PriceDirection,
        };
        let area = frame.area();
        let styles = settings.styles();
        frame.buffer_mut().set_style(area, styles.panel_base());
        if area.width < 24 || area.height < 10 {
            frame.render_widget(Paragraph::new("Kline needs more space"), area);
            return;
        }
        let block = Block::default()
            .title(format!(" {} ", self.title()))
            .borders(Borders::ALL)
            .style(styles.panel_base())
            .border_style(styles.border);
        let inner = block.inner(area);
        frame.render_widget(block, area);
        if self.candles.is_empty() {
            frame.render_widget(Paragraph::new("No candlesticks"), inner);
            return;
        }
        let selected = &self.candles[self.cursor.min(self.candles.len() - 1)];
        let previous = self
            .cursor
            .checked_sub(1)
            .and_then(|index| self.candles.get(index))
            .map(|candle| candle.close);
        let change = previous
            .filter(|price| !price.is_zero())
            .map(|price| (selected.close - price) / price * rust_decimal::Decimal::ONE_HUNDRED);
        let timestamp = selected
            .timestamp
            .format(&time::format_description::well_known::Rfc3339)
            .unwrap_or_default();
        let date = timestamp.get(..16).unwrap_or(&timestamp).replace('T', " ");
        let summary = format!(
            "{date} O {} H {} L {}",
            selected.open.normalize(),
            selected.high.normalize(),
            selected.low.normalize()
        );
        write_text(
            frame.buffer_mut(),
            inner.x,
            inner.y,
            &summary,
            inner.width as usize,
            settings.ambiguous_width,
            styles.text,
        );
        let summary = format!(
            "C {} V {} Change {}",
            selected.close.normalize(),
            selected.volume,
            change
                .map(|value| format!("{value:+.2}%"))
                .unwrap_or_else(|| "--".into())
        );
        write_text(
            frame.buffer_mut(),
            inner.x,
            inner.y + 1,
            &summary,
            inner.width as usize,
            settings.ambiguous_width,
            styles.text,
        );
        let glyph_width = width("┃", settings.ambiguous_width).max(1);
        let scale = self
            .candles
            .iter()
            .flat_map(|candle| [candle.high.scale(), candle.low.scale()])
            .max()
            .unwrap_or(2)
            .clamp(2, 8) as usize;
        let largest_label = self
            .candles
            .iter()
            .flat_map(|candle| [candle.high, candle.low])
            .map(|value| format!("{value:.scale$}").len())
            .max()
            .unwrap_or(6);
        let axis_width = largest_label.clamp(7, 24) + 1;
        let plot_width = (inner.width as usize).saturating_sub(axis_width);
        let capacity = (plot_width / glyph_width).max(1);
        self.visible_count.set(capacity);
        let mut start = self
            .viewport_start
            .get()
            .min(self.candles.len().saturating_sub(capacity));
        if self.cursor < start {
            start = self.cursor;
        }
        if self.cursor >= start + capacity {
            start = self.cursor + 1 - capacity;
        }
        self.viewport_start.set(start);
        let end = (start + capacity).min(self.candles.len());
        let visible = &self.candles[start..end];
        let plot_height = inner.height.saturating_sub(7).max(1);
        let plot_y = inner.y + 3;
        let mut minimum = visible
            .iter()
            .map(|candle| candle.low.to_f64().unwrap_or(0.0))
            .fold(f64::INFINITY, f64::min);
        let mut maximum = visible
            .iter()
            .map(|candle| candle.high.to_f64().unwrap_or(0.0))
            .fold(f64::NEG_INFINITY, f64::max);
        let closes: Vec<f64> = self
            .candles
            .iter()
            .map(|candle| candle.close.to_f64().unwrap_or(f64::NAN))
            .collect();
        let averages: Vec<Vec<Option<f64>>> = if self.show_averages {
            MOVING_AVERAGE_WINDOWS
                .iter()
                .map(|&window| tickloop_ui::charts::moving_average(&closes, window))
                .collect()
        } else {
            Vec::new()
        };
        if self.show_averages {
            let mut legend_x = inner.x;
            for (index, window) in MOVING_AVERAGE_WINDOWS.iter().enumerate() {
                let value = averages[index][self.cursor]
                    .map(|value| format!("{value:.2}"))
                    .unwrap_or_else(|| "--".into());
                let label = format!("MA{window} {value}  ");
                write_text(
                    frame.buffer_mut(),
                    legend_x,
                    inner.y + 2,
                    &label,
                    inner.right().saturating_sub(legend_x) as usize,
                    settings.ambiguous_width,
                    styles.moving_averages[index],
                );
                legend_x = legend_x.saturating_add(width(&label, settings.ambiguous_width) as u16);
            }
        }
        for values in &averages {
            for value in values[start..end].iter().flatten() {
                minimum = minimum.min(*value);
                maximum = maximum.max(*value);
            }
        }
        if (maximum - minimum).abs() < f64::EPSILON {
            let padding = maximum.abs().max(1.0) * 0.01;
            minimum -= padding;
            maximum += padding;
        }
        let chart_scale = f64::from(plot_height.saturating_sub(1)) / (maximum - minimum);
        // One price-to-plot projection: rows above the bottom of the plot, counted from zero.
        let project = |price: f64| (price - minimum) * chart_scale;
        let project_decimal =
            |value: rust_decimal::Decimal| project(value.to_f64().unwrap_or(minimum));
        for (index, candle) in visible.iter().enumerate() {
            let price_style = styles.price(PriceDirection::from_change(
                (candle.close - candle.open).to_f64().unwrap_or(0.0),
            ));
            let candle_x = inner.x + (index * glyph_width) as u16;
            for row in 0..plot_height {
                let glyph = tickloop_ui::charts::candle_glyph(
                    project_decimal(candle.open),
                    project_decimal(candle.high),
                    project_decimal(candle.low),
                    project_decimal(candle.close),
                    f64::from(plot_height - 1 - row),
                );
                let style = if start + index == self.cursor {
                    price_style.patch(styles.chart_cursor)
                } else {
                    price_style
                };
                write_text(
                    frame.buffer_mut(),
                    candle_x,
                    plot_y + row,
                    &glyph.to_string(),
                    glyph_width,
                    settings.ambiguous_width,
                    style,
                );
            }
        }
        if self.show_averages {
            let lines: Vec<Vec<(u16, u16)>> = averages
                .iter()
                .map(|values| {
                    values[start..end]
                        .iter()
                        .enumerate()
                        .filter_map(|(index, value)| {
                            value.map(|value| {
                                (
                                    (index * glyph_width * 2) as u16,
                                    ((maximum - value) / (maximum - minimum)
                                        * f64::from(plot_height * 4 - 1))
                                    .round()
                                    .clamp(0.0, f64::from(plot_height * 4 - 1))
                                        as u16,
                                )
                            })
                        })
                        .collect()
                })
                .collect();
            let cells = tickloop_ui::charts::braille_lines(&lines, plot_width as u16, plot_height);
            for (index, cell) in cells.iter().enumerate() {
                if let Some(cell) = cell {
                    let x = inner.x + (index % plot_width) as u16;
                    let y = plot_y + (index / plot_width) as u16;
                    write_text(
                        frame.buffer_mut(),
                        x,
                        y,
                        &cell.glyph.to_string(),
                        1,
                        settings.ambiguous_width,
                        styles.moving_averages[cell.line],
                    );
                }
            }
        }
        for row in [0, plot_height / 2, plot_height - 1] {
            let value = maximum
                - f64::from(row) / (f64::from(plot_height - 1).max(1.0)) * (maximum - minimum);
            let label = format!("{value:.scale$}");
            write_text(
                frame.buffer_mut(),
                inner.x + plot_width as u16 + 1,
                plot_y + row,
                &label,
                axis_width - 1,
                settings.ambiguous_width,
                styles.muted_text,
            );
        }
        let mut ordered_marks: Vec<&PriceMark> = self.marks.iter().collect();
        ordered_marks.sort_by(|left, right| right.price.total_cmp(&left.price));
        let highest_row = f64::from(plot_height - 1);
        let mark_rows: Vec<u16> = ordered_marks
            .iter()
            .map(|mark| {
                (highest_row - project(mark.price))
                    .round()
                    .clamp(0.0, highest_row) as u16
            })
            .collect();
        for row in &mark_rows {
            write_text(
                frame.buffer_mut(),
                inner.x,
                plot_y + row,
                &tickloop_ui::charts::PRICE_MARK_LINE
                    .to_string()
                    .repeat(plot_width),
                plot_width,
                settings.ambiguous_width,
                styles.price_mark,
            );
        }
        // Lines are drawn first so that no later one erases an earlier label;
        // several marks can project onto one row, so each label then takes the
        // next free row inside the plot.
        let mut occupied_label_rows: Vec<u16> = Vec::new();
        for (mark, row) in ordered_marks.iter().zip(&mark_rows) {
            let label = format!("{} {:.scale$} ", mark.label, mark.price);
            let label_width = width(&label, settings.ambiguous_width) as usize;
            let Some(label_row) = (*row..plot_height)
                .chain(0..*row)
                .find(|candidate| !occupied_label_rows.contains(candidate))
                .filter(|_| label_width <= plot_width)
            else {
                continue;
            };
            occupied_label_rows.push(label_row);
            write_text(
                frame.buffer_mut(),
                inner.x,
                plot_y + label_row,
                &label,
                label_width,
                settings.ambiguous_width,
                styles.price_mark,
            );
        }
        let max_volume = visible
            .iter()
            .map(|candle| candle.volume)
            .max()
            .unwrap_or(1)
            .max(1) as f64;
        let volume_y = plot_y + plot_height;
        for (index, candle) in visible.iter().enumerate() {
            let price_style = styles.price(PriceDirection::from_change(
                (candle.close - candle.open).to_f64().unwrap_or(0.0),
            ));
            let glyph = tickloop_ui::charts::volume_glyph(candle.volume as f64 / max_volume);
            write_text(
                frame.buffer_mut(),
                inner.x + (index * glyph_width) as u16,
                volume_y,
                &glyph.to_string(),
                glyph_width,
                settings.ambiguous_width,
                price_style,
            );
        }
        let date_y = volume_y + 1;
        let mut next_column = 0;
        for (index, candle) in visible.iter().enumerate() {
            let column = index * glyph_width;
            if column < next_column {
                continue;
            }
            let timestamp = candle
                .timestamp
                .format(&time::format_description::well_known::Rfc3339)
                .unwrap_or_default();
            let label = if matches!(
                self.request.period,
                crate::api::CandlePeriod::Day
                    | crate::api::CandlePeriod::Week
                    | crate::api::CandlePeriod::Month
            ) {
                timestamp.get(5..10).unwrap_or("")
            } else {
                timestamp.get(11..16).unwrap_or("")
            };
            if column + label.len() > plot_width {
                continue;
            }
            write_text(
                frame.buffer_mut(),
                inner.x + column as u16,
                date_y,
                label,
                label.len(),
                settings.ambiguous_width,
                styles.muted_text,
            );
            next_column = column + label.len() + 3;
        }
        write_text(
            frame.buffer_mut(),
            inner.x + ((self.cursor - start) * glyph_width) as u16,
            date_y + 1,
            "▲",
            glyph_width,
            settings.ambiguous_width,
            styles.chart_cursor,
        );
        let footer = format!(
            "1–8 Period · m {} · {}",
            if self.show_averages {
                "MA5/10/20"
            } else {
                "MA off"
            },
            if self.supports_push {
                "Live + 30s sync"
            } else {
                "30s refresh"
            }
        );
        frame.buffer_mut().set_style(
            ratatui::layout::Rect::new(inner.x, inner.bottom() - 1, inner.width, 1),
            styles.status_base(),
        );
        if let Some(editor) = &self.event_editor {
            editor.draw(frame, settings);
        }
        write_text(
            frame.buffer_mut(),
            inner.x,
            inner.bottom() - 1,
            &footer,
            inner.width as usize,
            settings.ambiguous_width,
            styles.status_base(),
        );
    }
}

pub async fn run_live<
    B: ratatui::backend::Backend,
    S: futures_util::Stream<Item = std::io::Result<crossterm::event::Event>> + Unpin,
>(
    args: &KlineArgs,
    api: &QuoteServices,
    settings: &Settings,
    terminal: &mut ratatui::Terminal<B>,
    mut input: S,
    updates: &mut TickStream,
) -> Result<()> {
    let request = request(args, MAX_CANDLES)?;
    if let Some(context) = ResearchContext::from_environment()? {
        let mut view = KlineView::new(request, Vec::new(), false);
        view.set_research_context(context)?;
        let (_market_sender, market) = tokio::sync::mpsc::unbounded_channel();
        return crate::runtime::run(&mut view, api, settings, terminal, input, market, updates)
            .await;
    }
    if let Some(config) = args.event.config()? {
        let mut view = KlineView::with_event(request, config)?;
        let Some(()) = crate::runtime::cancellable(view.load_event(api), &mut input).await? else {
            return Ok(());
        };
        let (_market_sender, market) = tokio::sync::mpsc::unbounded_channel();
        return crate::runtime::run(&mut view, api, settings, terminal, input, market, updates)
            .await;
    }
    let requested_at = tokio::time::Instant::now();
    let Some(candles) = crate::runtime::cancellable(api.candles(&request), &mut input).await?
    else {
        return Ok(());
    };
    let mut view = KlineView::new(request, candles, false);
    view.last_snapshot_request = Some(requested_at);
    let result = async {
        let Some(market) = crate::runtime::cancellable(api.take_events(), &mut input).await? else {
            return Ok(());
        };
        view.subscriptions.insert(view.request.period);
        let Some(supports_push) =
            crate::runtime::cancellable(api.subscribe_candles(&view.request), &mut input).await?
        else {
            return Ok(());
        };
        view.supports_push = supports_push;
        if !supports_push {
            view.subscriptions.remove(&view.request.period);
        }
        crate::runtime::run(&mut view, api, settings, terminal, input, market, updates).await
    }
    .await;
    let mut cleanup = Ok(());
    for period in &view.subscriptions {
        let unsubscribed = tokio::time::timeout(
            crate::runtime::CLEANUP_TIMEOUT,
            api.unsubscribe_candles(&view.request.symbol, *period),
        )
        .await
        .unwrap_or_else(|_| Err(Error::network("subscription cleanup timed out")));
        if cleanup.is_ok() {
            cleanup = unsubscribed;
        }
    }
    result.and(cleanup)
}
