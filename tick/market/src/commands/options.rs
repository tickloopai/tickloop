use std::{
    io::Write,
    sync::{Arc, RwLock},
    time::Duration,
};

use crossterm::event::KeyCode;
use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    widgets::{Block, Borders, Clear, Paragraph, Row, Table, TableState, Wrap},
};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::{
    args::{OptionsArgs, OutputFormat},
    config::Settings,
    options::{self, OptionsRequest, OptionsSnapshot, SnapshotHistory, SnapshotStore},
    output::TableData,
    research::{
        ResearchContext, ResearchEvidence, ResearchNavigation, ResearchTarget, SetResearchContext,
    },
    runtime::{LiveView, ViewState},
    tools::Tools,
};
use tick_provider::{
    api::{MarketEvent, QuoteServices},
    error::{Error, Result},
    options::OptionsApi,
};
use tick_sdk::{stream::TickStream, tools::ToolCall};

const REFRESH_INTERVAL: Duration = Duration::from_secs(60);
const SNAPSHOT_TOOL: &str = "snapshot";
const EXPIRATIONS_TOOL: &str = "expirations";
const EVENT_HISTORY_TOOL: &str = "event_history";
const TABLE_HEADER_ROWS: u16 = 1;
const HEADERS: &[&str] = &[
    "Strike",
    "Call bid/ask",
    "Put bid/ask",
    "C IV%",
    "P IV%",
    "C OI",
    "P OI",
    "C volume",
    "P volume",
];
type CurrentSnapshot = Arc<RwLock<Option<OptionsSnapshot>>>;

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ChainArgs {
    pub symbol: String,
    pub expiry: Option<String>,
    #[schemars(range(min = 1, max = 100))]
    pub strikes: Option<usize>,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SymbolArgs {
    pub symbol: String,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct HistoryArgs {
    pub symbol: String,
    #[schemars(range(min = 1, max = 100))]
    pub limit: Option<usize>,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ExpiryArgs {
    pub expiry: String,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct EmptyArgs {}

#[derive(Clone, Copy, Default, Deserialize, serde::Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ObservationView {
    #[default]
    Current,
    Before,
    After,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ObservationArgs {
    pub view: ObservationView,
}

pub fn tools() -> Tools {
    tools_for(None, None, Arc::new(RwLock::new(None)))
}

fn tools_for(
    api: Option<Arc<dyn OptionsApi>>,
    store: Option<SnapshotStore>,
    current: CurrentSnapshot,
) -> Tools {
    let expiration_api = api.clone();
    let event_store = store.clone();
    Tools::default()
        .query::<ChainArgs>("chain", "Read a US/HK option chain and a single-batch underlying/leg quote snapshot. Includes bid/ask, IV, open interest, volume, timestamps, and the same-expiry ATM straddle approximation; unavailable fields remain null.", move |_: &QuoteServices,args| {
            let api=api.clone();
            Box::pin(async move {
                let api=api.ok_or_else(||Error::usage("Options service is not attached"))?;
                let request=OptionsRequest::new(&args.symbol,args.expiry.as_deref(),args.strikes.unwrap_or(options::DEFAULT_STRIKES))?;
                serialize(&options::load(api.as_ref(),&request).await?)
            })
        })
        .query::<SymbolArgs>(EXPIRATIONS_TOOL, "Read available option expiration dates from this Tick's Futu OpenD connection.",move |_: &QuoteServices,args| {
            let api=expiration_api.clone();
            Box::pin(async move {
                let api=api.ok_or_else(||Error::usage("Options service is not attached"))?;
                serialize(&api.expirations(&tick_provider::options::normalize_symbol(&args.symbol)?).await?)
            })
        })
        .query::<EmptyArgs>(SNAPSHOT_TOOL, "Read the current Options Tick observation without a network request, including the original quote and observation times.",move |_: &QuoteServices,_| {
            let current=current.clone();
            Box::pin(async move { let snapshot=current.read().map_err(|_|Error::usage("Options snapshot lock failed"))?; serialize(snapshot.as_ref().ok_or_else(||Error::usage("no Options observation is available"))?) })
        })
        .query::<HistoryArgs>("history", "Read explicitly saved local option snapshots; reports missing history rather than reconstructing pre-event prices from current quotes.",move |_: &QuoteServices,args| {
            let store=store.clone();
            Box::pin(async move { serialize(&store.ok_or_else(||Error::usage("Options snapshot store is not attached"))?.history(&args.symbol,args.limit.unwrap_or(20))?) })
        })
        .action::<ExpiryArgs>("set_expiry", "Change the running Options Tick to an expiration returned for its underlying.")
        .query::<SetResearchContext>(EVENT_HISTORY_TOOL, "Read saved snapshots associated with this exact earnings event. Before/after is checked against original observation and quote timestamps; missing history stays missing, never reconstructed from current quotes.", move |_: &QuoteServices,args| {
            let store=event_store.clone();
            Box::pin(async move { serialize(&store.ok_or_else(||Error::usage("Options snapshot store is not attached"))?.event_history(&args.context)?) })
        })
        .action::<SetResearchContext>("set_context", "Follow this company and earnings period in the same instance. Shows the saved pre-event snapshot, or an explicit missing state. Current quotes are kept separate.")
        .action::<ObservationArgs>("set_observation", "Show current quotes, the saved before-event snapshot, or the saved after-event observation. This never reconstructs missing historical data.")
        .action::<EmptyArgs>("refresh", "Refresh this Tick's chain and quote snapshot.")
        .action::<EmptyArgs>("save_snapshot", "Save the currently observed Options snapshot locally, preserving the original timestamps for later pre/post-event research.")
        .recover_with("chain")
}

/// One observation as the Tick state carries it: the underlying, the ATM estimate and the span of
/// the chain, whose legs stay behind `chain_tool`.
fn snapshot_summary(snapshot: &OptionsSnapshot, chain_tool: &str) -> Value {
    json!({
        "schema_version": snapshot.schema_version, "source": snapshot.source,
        "symbol": snapshot.symbol, "expiry": snapshot.expiry, "observed_at": snapshot.observed_at,
        "quote_time_zone": snapshot.quote_time_zone, "underlying": snapshot.underlying,
        "atm": snapshot.atm, "warnings": snapshot.warnings,
        "research_context": snapshot.research_context,
        "expirations": {"count": snapshot.expirations.len(), "tool": EXPIRATIONS_TOOL},
        "chain": {"strikes": snapshot.rows.len(),
            "lowest": snapshot.rows.first().map(|row| row.strike),
            "highest": snapshot.rows.last().map(|row| row.strike), "tool": chain_tool},
    })
}

fn event_history_summary(history: &options::EventSnapshotHistory) -> Value {
    let mut summary = serialize(history).unwrap_or(Value::Null);
    for (key, snapshot) in [("before", &history.before), ("after", &history.after)] {
        summary[key] = json!(
            snapshot
                .as_ref()
                .map(|snapshot| snapshot_summary(snapshot, EVENT_HISTORY_TOOL))
        );
    }
    summary
}

fn serialize(value: &impl serde::Serialize) -> Result<Value> {
    serde_json::to_value(value).map_err(|error| Error::usage(error.to_string()))
}

pub fn table(snapshot: &OptionsSnapshot) -> TableData {
    TableData {
        headers: HEADERS.to_vec(),
        rows: snapshot
            .rows
            .iter()
            .map(|pair| {
                let call = pair.call.as_ref().and_then(|leg| leg.quote.as_ref());
                let put = pair.put.as_ref().and_then(|leg| leg.quote.as_ref());
                vec![
                    json!(pair.strike),
                    json!(book(call)),
                    json!(book(put)),
                    json!(call.and_then(|quote| quote.implied_volatility_percent)),
                    json!(put.and_then(|quote| quote.implied_volatility_percent)),
                    json!(call.and_then(|quote| quote.open_interest)),
                    json!(put.and_then(|quote| quote.open_interest)),
                    json!(call.and_then(|quote| quote.volume)),
                    json!(put.and_then(|quote| quote.volume)),
                ]
            })
            .collect(),
    }
}

fn book(quote: Option<&tick_provider::options::MarketQuote>) -> String {
    quote
        .map(|quote| format!("{}/{}", amount(quote.bid), amount(quote.ask)))
        .unwrap_or_else(|| "—".into())
}

fn amount(value: Option<f64>) -> String {
    value
        .map(|value| format!("{value:.2}"))
        .unwrap_or_else(|| "—".into())
}

pub async fn run_once(
    args: &OptionsArgs,
    api: Arc<dyn OptionsApi>,
    settings: &Settings,
    color: bool,
    output: &mut impl Write,
    updates: &mut TickStream,
) -> Result<()> {
    let request = OptionsRequest::new(&args.symbol, args.expiry.as_deref(), args.strikes)?;
    let snapshot = options::load(api.as_ref(), &request).await?;
    updates.publish(snapshot_summary(&snapshot, SNAPSHOT_TOOL))?;
    if args.output.format == OutputFormat::Json {
        serde_json::to_writer(&mut *output, &serialize(&snapshot)?)
            .map_err(|error| Error::usage(error.to_string()))?;
        writeln!(output)?;
    } else {
        writeln!(
            output,
            "{} Options · {} · {} · observed {}",
            snapshot.symbol, snapshot.expiry, snapshot.source, snapshot.observed_at
        )?;
        writeln!(output, "{}", estimate_text(&snapshot))?;
        table(&snapshot).write(OutputFormat::Table, settings, color, output)?;
        for warning in &snapshot.warnings {
            writeln!(output, "{warning}")?;
        }
    }
    Ok(())
}

pub struct OptionsView {
    request: OptionsRequest,
    snapshot: OptionsSnapshot,
    api: Arc<dyn OptionsApi>,
    store: SnapshotStore,
    current: CurrentSnapshot,
    history: Option<SnapshotHistory>,
    detail: bool,
    detail_scroll: u16,
    notice: Option<String>,
    research: Option<ResearchContext>,
    event_history: Option<options::EventSnapshotHistory>,
    observation: ObservationView,
    current_available: bool,
    navigation: Option<ResearchNavigation>,
}

impl OptionsView {
    pub fn for_research(
        context: ResearchContext,
        api: Arc<dyn OptionsApi>,
        store: SnapshotStore,
    ) -> Result<Self> {
        let request = OptionsRequest::new(&context.symbol, None, options::DEFAULT_STRIKES)?;
        let snapshot = OptionsSnapshot {
            schema_version: 1,
            source: api.source().into(),
            symbol: request.symbol.clone(),
            expiry: String::new(),
            expirations: vec![],
            observed_at: String::new(),
            quote_time_zone: String::new(),
            underlying: tick_provider::options::MarketQuote {
                symbol: request.symbol.clone(),
                ..Default::default()
            },
            rows: vec![],
            atm: None,
            warnings: vec![],
            research_context: None,
        };
        let mut view = Self::new(request, snapshot, api, store);
        view.current_available = false;
        view.set_research_context(context)?;
        Ok(view)
    }
    pub fn new(
        request: OptionsRequest,
        snapshot: OptionsSnapshot,
        api: Arc<dyn OptionsApi>,
        store: SnapshotStore,
    ) -> Self {
        let current = Arc::new(RwLock::new(Some(snapshot.clone())));
        Self {
            request,
            snapshot,
            api,
            store,
            current,
            history: None,
            detail: false,
            detail_scroll: 0,
            notice: None,
            research: None,
            event_history: None,
            observation: ObservationView::Current,
            current_available: true,
            navigation: None,
        }
    }

    pub fn set_research_context(&mut self, context: ResearchContext) -> Result<()> {
        context.validate()?;
        let request = OptionsRequest::new(&context.symbol, None, self.request.strikes)?;
        let history = self.store.event_history(&context)?;
        self.current_available = self.current_available && self.snapshot.symbol == request.symbol;
        self.request = request;
        if self.current_available {
            self.snapshot.bind_event(Some(&context));
        }
        *self
            .current
            .write()
            .map_err(|_| Error::usage("Options snapshot lock failed"))? =
            self.current_available.then(|| self.snapshot.clone());
        self.research = Some(context);
        self.event_history = Some(history);
        self.observation = ObservationView::Before;
        self.history = None;
        self.detail = false;
        self.detail_scroll = 0;
        self.notice = None;
        self.navigation = None;
        Ok(())
    }

    fn shown_snapshot(&self) -> Option<&OptionsSnapshot> {
        match self.observation {
            ObservationView::Current => self.current_available.then_some(&self.snapshot),
            ObservationView::Before => self.event_history.as_ref()?.before.as_ref(),
            ObservationView::After => self.event_history.as_ref()?.after.as_ref(),
        }
    }

    fn observation_label(&self) -> &'static str {
        match self.observation {
            ObservationView::Current => "CURRENT quotes (not pre-event pricing)",
            ObservationView::Before => "SAVED BEFORE event",
            ObservationView::After => {
                "SAVED AFTER event (quote-time interval, not isolated event return)"
            }
        }
    }

    async fn reload(&mut self, expiry: Option<String>) -> Result<()> {
        let request = OptionsRequest {
            expiry,
            ..self.request.clone()
        };
        let mut snapshot = options::load(self.api.as_ref(), &request).await?;
        snapshot.bind_event(self.research.as_ref());
        *self
            .current
            .write()
            .map_err(|_| Error::usage("Options snapshot lock failed"))? = Some(snapshot.clone());
        self.snapshot = snapshot;
        self.request = request;
        self.current_available = true;
        self.notice = None;
        Ok(())
    }

    fn save(&mut self) -> Result<PathResult> {
        if !self.current_available {
            return Err(Error::usage(
                "current quotes unavailable for this symbol; refresh before saving",
            ));
        }
        let path = self.store.save(&self.snapshot)?;
        if let Some(context) = &self.research {
            self.event_history = Some(self.store.event_history(context)?);
        }
        self.notice = Some(format!("Saved {}", path.display()));
        Ok(PathResult {
            path: path.display().to_string(),
            observed_at: self.snapshot.observed_at.clone(),
        })
    }

    fn detail_text(&self, selected: usize) -> String {
        if let Some(history) = &self.history {
            return history
                .snapshots
                .get(selected)
                .map(|snapshot| serde_json::to_string_pretty(snapshot).unwrap_or_default())
                .unwrap_or_else(|| history.note.into());
        }
        let Some(snapshot) = self.shown_snapshot() else {
            return "No saved snapshot for this event and observation phase. Current quotes are not substituted.".into();
        };
        let Some(row) = snapshot.rows.get(selected) else {
            return "No option contract selected".into();
        };
        let mut lines = vec![format!(
            "{} · expiry {} · strike {}",
            snapshot.symbol, snapshot.expiry, row.strike
        )];
        for (label, leg) in [("Call", &row.call), ("Put", &row.put)] {
            lines.push(String::new());
            if let Some(leg) = leg {
                lines.push(format!("{label} {}", leg.contract.symbol));
                if let Some(quote) = &leg.quote {
                    lines.extend([
                        format!(
                            "Bid / Ask {} · Last {}",
                            book(Some(quote)),
                            amount(quote.last)
                        ),
                        format!(
                            "IV {}% · OI {} · Volume {}",
                            amount(quote.implied_volatility_percent),
                            quote
                                .open_interest
                                .map(|value| value.to_string())
                                .unwrap_or_else(|| "—".into()),
                            quote
                                .volume
                                .map(|value| value.to_string())
                                .unwrap_or_else(|| "—".into())
                        ),
                        format!(
                            "Quote time {} ({})",
                            quote.quote_time.as_deref().unwrap_or("missing"),
                            snapshot.quote_time_zone
                        ),
                    ]);
                } else {
                    lines.push("Quote missing; no last-price or zero substitution".into());
                }
            } else {
                lines.push(format!("{label}: contract missing or ambiguous"));
            }
        }
        lines.push(format!("\nObserved {} UTC", snapshot.observed_at));
        lines.push("\nATM approximation uses both bid-ask midpoints, divided by underlying last. Quote times may differ; no freshness guarantee. This is not a direction forecast or probability interval.".into());
        lines.join("\n")
    }
}

#[derive(serde::Serialize)]
struct PathResult {
    path: String,
    observed_at: String,
}

fn estimate_text(snapshot: &OptionsSnapshot) -> String {
    match &snapshot.atm {
        Some(atm)=>format!("ATM {} · C mid {:.2} + P mid {:.2} = {:.2} / spot {:.2} → ±{:.2}% · through expiry, not direction",atm.strike,atm.call_midpoint,atm.put_midpoint,atm.premium,atm.underlying_price,atm.move_percent),
        None=>"ATM move unavailable · both valid bid/ask pairs, standard contracts and quote times required".into(),
    }
}

#[async_trait::async_trait]
impl LiveView for OptionsView {
    fn tick(&self) -> crate::metadata::TickKind {
        crate::metadata::TickKind::Options
    }
    fn title(&self) -> String {
        if let Some(context) = &self.research {
            return format!(
                "{} · Options · {}",
                context.title(),
                self.observation_label()
            );
        }
        format!(
            "{} · Options · {} · {}",
            self.snapshot.symbol, self.snapshot.expiry, self.snapshot.source
        )
    }
    fn table(&self) -> TableData {
        if let Some(history) = &self.history {
            TableData {
                headers: vec!["Observed UTC", "Expiry", "Source", "ATM move %"],
                rows: history
                    .snapshots
                    .iter()
                    .map(|snapshot| {
                        vec![
                            json!(snapshot.observed_at),
                            json!(snapshot.expiry),
                            json!(snapshot.source),
                            json!(snapshot.atm.as_ref().map(|atm| atm.move_percent)),
                        ]
                    })
                    .collect(),
            }
        } else {
            self.shown_snapshot().map(table).unwrap_or(TableData {
                headers: HEADERS.to_vec(),
                rows: vec![],
            })
        }
    }
    fn content(&self) -> Value {
        if let Some(context) = &self.research {
            return json!({"symbol":self.request.symbol,"research_context":context,
                "current_snapshot":self.current_available.then(|| snapshot_summary(&self.snapshot, SNAPSHOT_TOOL)),
                "event_history":self.event_history.as_ref().map(event_history_summary)});
        }
        snapshot_summary(&self.snapshot, SNAPSHOT_TOOL)
    }
    fn research_context(&self) -> Option<ResearchContext> {
        self.research.clone()
    }
    fn take_navigation(&mut self) -> Option<ResearchNavigation> {
        self.navigation.take()
    }
    fn tools(&self) -> Tools {
        tools_for(
            Some(self.api.clone()),
            Some(self.store.clone()),
            self.current.clone(),
        )
    }
    fn priority(&self) -> &[usize] {
        &[0, 1, 2, 3, 4, 5, 6, 7, 8]
    }
    fn on_market(&mut self, _: &MarketEvent) {}
    fn refresh_interval(&self) -> Option<Duration> {
        matches!(self.observation, ObservationView::Current).then_some(REFRESH_INTERVAL)
    }
    async fn refresh(&mut self, _: &QuoteServices) -> Result<()> {
        self.reload(self.request.expiry.clone()).await
    }
    async fn act(&mut self, call: ToolCall) -> Result<Value> {
        match call.tool.as_str() {
            "set_context" => {
                self.set_research_context(call.arguments::<SetResearchContext>()?.context)?;
                Ok(self.content())
            }
            "set_observation" => {
                self.observation = call.arguments::<ObservationArgs>()?.view;
                self.history = None;
                self.detail = false;
                if matches!(self.observation, ObservationView::Current) && !self.current_available {
                    self.reload(None).await?;
                }
                Ok(self.content())
            }
            "set_expiry" => {
                let args: ExpiryArgs = call.arguments()?;
                self.reload(Some(args.expiry)).await?;
                self.history = None;
                self.detail = false;
                Ok(self.content())
            }
            "refresh" => {
                let _: EmptyArgs = call.arguments()?;
                self.reload(self.request.expiry.clone()).await?;
                Ok(self.content())
            }
            "save_snapshot" => {
                let _: EmptyArgs = call.arguments()?;
                serialize(&self.save()?)
            }
            _ => Err(Error::usage(format!(
                "unknown Options action {:?}",
                call.tool
            ))),
        }
    }
    async fn on_key(
        &mut self,
        key: KeyCode,
        _: &QuoteServices,
        state: &mut ViewState,
    ) -> Result<bool> {
        if key == KeyCode::Backspace {
            if self.detail {
                self.detail = false;
                self.detail_scroll = 0;
            } else if self.history.take().is_some() {
                state.selected = 0;
                state.offset = 0;
            } else {
                self.notice = None;
            }
            return Ok(true);
        }
        if self.detail {
            match key {
                KeyCode::Char('j') | KeyCode::Down => {
                    self.detail_scroll = self.detail_scroll.saturating_add(1)
                }
                KeyCode::Char('k') | KeyCode::Up => {
                    self.detail_scroll = self.detail_scroll.saturating_sub(1)
                }
                KeyCode::PageDown => self.detail_scroll = self.detail_scroll.saturating_add(10),
                KeyCode::PageUp => self.detail_scroll = self.detail_scroll.saturating_sub(10),
                _ => (),
            }
            return Ok(true);
        }
        match key {
            KeyCode::Char('a') if self.research.is_some() => {
                let mut context = self.research.clone().unwrap();
                context.evidence = Some(ResearchEvidence {
                    label: self.observation_label().into(),
                    text: serde_json::to_string(&json!({"observation":self.observation_label(),"snapshot":self.shown_snapshot(),"missing":self.shown_snapshot().is_none(),"saved_spot_change_percent":self.event_history.as_ref().and_then(|history|history.saved_spot_change_percent),"comparison_basis":self.event_history.as_ref().map(|history|history.comparison_basis)})).unwrap_or_default(),
                    origin: Some(self.api.source().into()), ..Default::default()
                });
                self.navigation = Some(ResearchNavigation {
                    target: ResearchTarget::Chat,
                    context,
                });
            }
            KeyCode::Char('c') | KeyCode::Char('b') | KeyCode::Char('p')
                if self.research.is_some() =>
            {
                self.observation = match key {
                    KeyCode::Char('c') => ObservationView::Current,
                    KeyCode::Char('b') => ObservationView::Before,
                    _ => ObservationView::After,
                };
                self.history = None;
                state.selected = 0;
                state.offset = 0;
                if matches!(self.observation, ObservationView::Current) && !self.current_available {
                    self.reload(None).await?;
                }
            }
            KeyCode::Enter => {
                self.detail = true;
                self.detail_scroll = 0;
            }
            KeyCode::Char('h') => {
                self.history = Some(if let Some(history) = &self.event_history {
                    let snapshots: Vec<_> = [&history.before, &history.after]
                        .into_iter()
                        .filter_map(Clone::clone)
                        .collect();
                    options::SnapshotHistory {
                        missing: snapshots.is_empty(),
                        snapshots,
                        invalid_files: history.invalid_files.clone(),
                        note: "Saved snapshots for this earnings event only; b before, p after, c current quotes.",
                    }
                } else {
                    self.store
                        .history(&self.request.symbol, options::MAX_HISTORY)?
                });
                state.selected = 0;
                state.offset = 0;
            }
            KeyCode::Char('s') => {
                self.save()?;
            }
            KeyCode::Char('r') => self.reload(self.request.expiry.clone()).await?,
            KeyCode::Char('[') | KeyCode::Char(']')
                if self.history.is_none()
                    && matches!(self.observation, ObservationView::Current)
                    && self.current_available =>
            {
                let index = self
                    .snapshot
                    .expirations
                    .iter()
                    .position(|expiry| expiry.date == self.snapshot.expiry)
                    .unwrap_or_default();
                let next = if key == KeyCode::Char('[') {
                    index.saturating_sub(1)
                } else {
                    (index + 1).min(self.snapshot.expirations.len().saturating_sub(1))
                };
                if next != index {
                    let expiry = self.snapshot.expirations[next].date.clone();
                    self.reload(Some(expiry)).await?;
                    state.selected = 0;
                    state.offset = 0;
                }
            }
            _ => return Ok(false),
        }
        Ok(true)
    }
    fn help(&self) -> &'static str {
        "q / Ctrl+C  Quit\nBackspace  Close details, then history; stay at root\nj / k / Ctrl+N / Ctrl+P  Move\nEnter  Contract details or saved snapshot\n[ / ]  Previous / next expiry\nr  Refresh current expiry\ns  Save current observation locally\nh  Read saved history\nb / p / c  Saved before / saved after / current\na  Ask Chat with this event and observation\n?  Help\nFutu OpenD: quotes only; no trading\nMissing pre-event snapshots remain missing"
    }
    fn draw(&self, frame: &mut Frame<'_>, state: &mut ViewState, settings: &Settings) {
        let area = frame.area();
        let styles = settings.styles();
        frame.buffer_mut().set_style(area, styles.panel_base());
        let description = if let Some(history) = &self.history {
            format!(
                "Local saved snapshots: {} · {}\n{}",
                history.snapshots.len(),
                if history.missing {
                    "MISSING"
                } else {
                    "available"
                },
                if history.invalid_files.is_empty() {
                    history.note.to_owned()
                } else {
                    format!(
                        "{} invalid files: {}",
                        history.invalid_files.len(),
                        history.invalid_files[0]
                    )
                }
            )
        } else if let Some(snapshot) = self.shown_snapshot() {
            format!(
                "Spot {} · quote {} ({})\n{}\nObserved {}",
                amount(snapshot.underlying.last),
                snapshot
                    .underlying
                    .quote_time
                    .as_deref()
                    .unwrap_or("missing"),
                snapshot.quote_time_zone,
                estimate_text(snapshot),
                snapshot.observed_at
            )
        } else if matches!(self.observation, ObservationView::Current) {
            "Current quotes unavailable. Start or reconnect Futu OpenD, then r to retry.\nb / p still open this event's saved observations.".into()
        } else {
            format!(
                "{}: MISSING\nNo snapshot with verified observation/quote times for this event.\nCurrent prices are never used to reconstruct pre-event pricing.",
                self.observation_label()
            )
        };
        let description = if let Some(history) = &self.event_history {
            format!(
                "{description}\nSaved spot change {}% · {} unclassified\n{}",
                amount(history.saved_spot_change_percent),
                history.unclassified_count,
                history.timing_basis
            )
        } else {
            description
        };
        let body = Rect::new(area.x, area.y, area.width, area.height.saturating_sub(1));
        let description = Paragraph::new(description)
            .style(styles.text)
            .wrap(Wrap { trim: false });
        let description_rows = if self.research.is_some() { 5 } else { 3 }
            .max(u16::try_from(description.line_count(body.width)).unwrap_or(u16::MAX));
        let note=self.notice.as_deref().unwrap_or(if self.research.is_some() { "b before · p after · c current · a Ask Chat · Enter details\ns save CURRENT · r refresh current · h event history · Backspace back" } else { "[ ] expiry · Enter details · r refresh · s save · h history · Backspace back\nNo last-price substitution; bid/ask midpoint is a pricing approximation." });
        let note = Paragraph::new(note)
            .style(styles.muted_text)
            .wrap(Wrap { trim: false });
        let note_rows = 2.max(u16::try_from(note.line_count(body.width)).unwrap_or(u16::MAX));
        let parts = Layout::vertical([
            Constraint::Length(1),
            Constraint::Length(description_rows),
            Constraint::Min(3),
            Constraint::Length(note_rows),
        ])
        .split(body);
        frame.render_widget(
            Paragraph::new(self.title()).style(styles.panel_base().patch(styles.title)),
            parts[0],
        );
        frame.render_widget(description, parts[1]);
        let table = self.table();
        state.selected = state.selected.min(table.rows.len().saturating_sub(1));
        let columns = if self.history.is_some() {
            4
        } else if area.width >= 112 {
            9
        } else if area.width >= 80 {
            7
        } else {
            5
        };
        let widths = if self.history.is_some() {
            vec![
                Constraint::Percentage(45),
                Constraint::Percentage(22),
                Constraint::Percentage(15),
                Constraint::Percentage(18),
            ]
        } else {
            (0..columns)
                .map(|index| {
                    Constraint::Length(match index {
                        0 => 8,
                        1 | 2 => 14,
                        3 | 4 => 8,
                        _ => 10,
                    })
                })
                .collect()
        };
        let rows = table.rows.iter().map(|row| {
            Row::new(
                row.iter()
                    .take(columns)
                    .map(|value| match value {
                        Value::Null => "—".into(),
                        Value::String(text) => text.clone(),
                        _ => value.to_string(),
                    })
                    .collect::<Vec<String>>(),
            )
        });
        let widget = Table::new(rows, widths)
            .header(Row::new(table.headers.iter().take(columns).copied()).style(styles.muted_text))
            .row_highlight_style(styles.selection)
            .highlight_symbol("› ");
        let mut table_state = TableState::default().with_selected(Some(state.selected));
        frame.render_stateful_widget(widget, parts[2], &mut table_state);
        state.focus_row =
            Some(parts[2].y + TABLE_HEADER_ROWS + (state.selected - table_state.offset()) as u16);
        frame.render_widget(note, parts[3]);
        if self.detail {
            let popup = Rect::new(
                area.x + 1,
                area.y + 1,
                area.width.saturating_sub(2),
                area.height.saturating_sub(3),
            );
            frame.render_widget(Clear, popup);
            frame.render_widget(
                Paragraph::new(self.detail_text(state.selected))
                    .style(styles.overlay_base())
                    .wrap(Wrap { trim: false })
                    .scroll((self.detail_scroll, 0))
                    .block(
                        Block::default()
                            .title(" Contract / saved snapshot · Backspace back ")
                            .borders(Borders::ALL)
                            .border_style(styles.border),
                    ),
                popup,
            );
        }
    }
}

pub async fn run_live<
    B: ratatui::backend::Backend,
    S: futures_util::Stream<Item = std::io::Result<crossterm::event::Event>> + Unpin,
>(
    args: &OptionsArgs,
    api: Arc<dyn OptionsApi>,
    settings: &Settings,
    terminal: &mut ratatui::Terminal<B>,
    mut input: S,
    updates: &mut TickStream,
) -> Result<()> {
    let store = SnapshotStore::new(args.snapshot_dir.clone())?;
    let mut view = if let Some(context) = ResearchContext::from_environment()? {
        OptionsView::for_research(context, api, store)?
    } else {
        let request = OptionsRequest::new(&args.symbol, args.expiry.as_deref(), args.strikes)?;
        let Some(snapshot) =
            crate::runtime::cancellable(options::load(api.as_ref(), &request), &mut input).await?
        else {
            return Ok(());
        };
        OptionsView::new(request, snapshot, api, store)
    };
    let (_sender, market) = tokio::sync::mpsc::unbounded_channel();
    crate::runtime::run(
        &mut view,
        &crate::runtime::NoMarketApi,
        settings,
        terminal,
        input,
        market,
        updates,
    )
    .await
}
