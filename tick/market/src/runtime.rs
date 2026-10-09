use crate::{canvas::CanvasBackend, config::Settings, output::TableData, tools::Tools};
use async_trait::async_trait;
use crossterm::event::{Event, KeyCode, KeyEventKind, KeyModifiers};
use futures_util::{Stream, StreamExt, stream::FuturesUnordered};
use ratatui::{
    Frame, Terminal,
    backend::Backend,
    layout::{Rect, Size},
    widgets::{Block, Borders, Clear, Paragraph, Wrap},
};
use serde_json::{Value, json};
use tick_protocol::{INVALID_PARAMS, TOOL_FAILED};
use tick_provider::{
    api::{ConnectionState, MarketEvent, QuoteServices},
    error::{Error, Result},
};
use tick_sdk::{
    stream::{RpcError, TickStream, ToolRequest},
    tools::{ToolCall, ToolDispatch},
};
use tickloop_ui::{
    format::{pad, width, write_text},
    scroll::{ScrollViewport, ViewportLayout},
};

const FRAME_INTERVAL: std::time::Duration = std::time::Duration::from_millis(33);
const WIDE_PANEL_COLUMNS: u16 = 64;
const TALL_PANEL_ROWS: u16 = 10;
pub const CLEANUP_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(1);
pub const REFRESH_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);
/// A Tick Tool call runs beside the view, so both the number in flight and how
/// long each one may take are bounded.
pub const TOOL_CALL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);
pub const MAX_CONCURRENT_TOOL_CALLS: usize = 4;

pub fn is_exit(event: &Event) -> bool {
    matches!(event, Event::Key(key) if key.kind != KeyEventKind::Release && (key.code == KeyCode::Char('q') || (key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL))))
}

pub struct NoMarketApi;

#[async_trait]
impl tick_provider::api::QuoteApi for NoMarketApi {
    async fn quotes(&self, _: &[String]) -> Result<Vec<tick_provider::api::Quote>> {
        Err(Error::usage("this Tick uses its own data services"))
    }
}

pub async fn cancellable<T, S: Stream<Item = std::io::Result<Event>> + Unpin>(
    future: impl std::future::Future<Output = Result<T>>,
    input: &mut S,
) -> Result<Option<T>> {
    cancellable_unless_editing(future, input, false).await
}

/// Like [`cancellable`], except that while an in-Panel editor is open q and Ctrl+C belong to it:
/// only the end of input cancels, and keys typed meanwhile are dropped as they are for any wait.
async fn cancellable_unless_editing<T, S: Stream<Item = std::io::Result<Event>> + Unpin>(
    future: impl std::future::Future<Output = Result<T>>,
    input: &mut S,
    editing: bool,
) -> Result<Option<T>> {
    tokio::pin!(future);
    loop {
        tokio::select! {
            biased;
            result = &mut future => return result.map(Some),
            event = input.next() => match event {
                None => return Ok(None),
                Some(Err(error)) => return Err(error.into()),
                Some(Ok(event)) if !editing && is_exit(&event) => return Ok(None),
                _ => (),
            },
        }
    }
}

pub struct ViewState {
    pub selected: usize,
    pub offset: usize,
    pub help: bool,
    pub connection: ConnectionState,
    pub last_error: Option<String>,
    /// The canvas column and row of what the view has focused in this frame; a scrolling
    /// Panel keeps them in view.
    pub focus_column: Option<u16>,
    pub focus_row: Option<u16>,
}

impl Default for ViewState {
    fn default() -> Self {
        Self {
            selected: 0,
            offset: 0,
            help: false,
            connection: ConnectionState::Connected,
            last_error: None,
            focus_column: None,
            focus_row: None,
        }
    }
}

impl ViewState {
    fn failed(&mut self, error: Error) -> Result<()> {
        if error.code == tick_provider::exit_code::NETWORK {
            self.connection = ConnectionState::Disconnected;
            self.last_error = Some(error.message);
            Ok(())
        } else if error.code == tick_provider::exit_code::NOT_AUTHORIZED
            && !error.message.contains(tick_provider::error::AUTH_HINT)
        {
            Err(Error::auth(error.message))
        } else {
            Err(error)
        }
    }

    fn received_data(&mut self) {
        self.connection = ConnectionState::Connected;
        self.last_error = None;
    }

    fn availability(&self) -> &str {
        match self.connection {
            ConnectionState::Connected => "Data received",
            ConnectionState::Reconnecting => "Recovering",
            ConnectionState::Disconnected if self.last_error.is_some() => "Unavailable · retry",
            ConnectionState::Disconnected => "Disconnected",
        }
    }
}

#[async_trait]
pub trait LiveView: Send {
    fn research_context(&self) -> Option<crate::research::ResearchContext> {
        None
    }
    fn take_navigation(&mut self) -> Option<crate::research::ResearchNavigation> {
        None
    }
    fn tools(&self) -> Tools {
        crate::tools::for_kind(self.tick())
    }
    fn tick(&self) -> crate::metadata::TickKind {
        crate::metadata::TickKind::Quote
    }
    /// The Panel this view needs now: the Tick's declared minimum unless the view is in a
    /// mode with a wider layout of its own.
    fn minimum_panel_size(&self) -> crate::metadata::MinimumPanelSize {
        self.tick().minimum_panel_size()
    }
    fn title(&self) -> String;
    fn table(&self) -> TableData;
    fn content(&self) -> serde_json::Value {
        self.table().json()
    }
    fn refresh_interval(&self) -> Option<std::time::Duration> {
        None
    }
    fn refresh_on_recovery(&self) -> bool {
        false
    }
    async fn refresh(&mut self, _api: &QuoteServices) -> Result<()> {
        Ok(())
    }
    fn priority(&self) -> &[usize];
    fn on_market(&mut self, event: &MarketEvent);
    async fn on_key(
        &mut self,
        _key: KeyCode,
        _api: &QuoteServices,
        _state: &mut ViewState,
    ) -> Result<bool> {
        Ok(false)
    }
    /// Applies an action Tick Tool. The runtime publishes the next Tick Update
    /// by itself when `content()` changed, so a view never publishes here.
    async fn act(&mut self, call: ToolCall) -> Result<Value> {
        Err(Error::usage(format!(
            "this Tick offers no action Tick Tool {:?}",
            call.tool
        )))
    }
    fn help(&self) -> &'static str {
        "q / Ctrl+C  Quit\n?  Help\nj / k / Ctrl+N / Ctrl+P / arrows  Move\nPageUp / PageDown  Page\ng / G  First / last"
    }
    /// While an in-Panel editor is open the view owns every printable key and Backspace, and
    /// Ctrl+C cancels the edit instead of quitting.
    fn captures_text(&self) -> bool {
        false
    }
    /// Closes the open in-Panel editor without applying it.
    fn cancel_text(&mut self) {}
    fn draw(&self, frame: &mut Frame<'_>, state: &mut ViewState, settings: &Settings) {
        draw_table(
            frame,
            &self.title(),
            &self.table(),
            self.priority(),
            state,
            settings,
        );
    }
}

type RunningToolCall<'a> =
    futures_util::future::BoxFuture<'a, (Value, std::result::Result<Value, RpcError>)>;

/// An accepted `tick/call`: a query already running beside the view, or an
/// action the loop still has to apply on the UI path.
enum AcceptedToolCall<'a> {
    Query(RunningToolCall<'a>),
    Action { id: Value, call: ToolCall },
}

fn tool_failure(error: Error) -> RpcError {
    RpcError::new(TOOL_FAILED, error.message).with_data(json!({"code": error.code}))
}

/// Checks one `tick/call` against the declared catalog and either starts it
/// beside the view or hands the action on; otherwise produces the error to
/// answer it with. A query never borrows the view, so the Panel keeps drawing
/// while a market round trip is in flight.
fn accept_tool_call<'a>(
    tools: &Tools,
    services: &'a QuoteServices,
    call: ToolRequest,
    calls_in_flight: usize,
) -> std::result::Result<AcceptedToolCall<'a>, (Value, RpcError)> {
    let Some(tool) = tools.get(&call.tool) else {
        return Err((call.id, RpcError::unknown_tool(&call.tool)));
    };
    if !tool.mutates() && calls_in_flight >= MAX_CONCURRENT_TOOL_CALLS {
        return Err((
            call.id,
            RpcError::new(
                TOOL_FAILED,
                format!(
                    "this Tick already runs {MAX_CONCURRENT_TOOL_CALLS} Tick Tool calls; retry when one finishes"
                ),
            ),
        ));
    }
    let dispatch = tool.prepare(services, call.args).map_err(|message| {
        (
            call.id.clone(),
            RpcError::new(INVALID_PARAMS, message)
                .with_data(json!({"schema": tool.input_schema()})),
        )
    })?;
    let id = call.id;
    Ok(match dispatch {
        ToolDispatch::Action(call) => AcceptedToolCall::Action { id, call },
        ToolDispatch::Query(running) => AcceptedToolCall::Query(Box::pin(async move {
            let result = tokio::time::timeout(TOOL_CALL_TIMEOUT, running)
                .await
                .unwrap_or_else(|_| Err(Error::network("Tick Tool call timed out")));
            (id, result.map_err(tool_failure))
        })),
    })
}

async fn refresh_cancellable<V: LiveView, S: Stream<Item = std::io::Result<Event>> + Unpin>(
    view: &mut V,
    api: &QuoteServices,
    input: &mut S,
) -> Result<Option<()>> {
    let editing = view.captures_text();
    cancellable_unless_editing(
        async {
            tokio::time::timeout(REFRESH_TIMEOUT, view.refresh(api))
                .await
                .unwrap_or_else(|_| Err(Error::network("refresh timed out")))
        },
        input,
        editing,
    )
    .await
}

pub async fn run<V: LiveView, B: Backend, S: Stream<Item = std::io::Result<Event>> + Unpin>(
    view: &mut V,
    api: &QuoteServices,
    settings: &Settings,
    terminal: &mut Terminal<B>,
    mut input: S,
    mut market: tokio::sync::mpsc::UnboundedReceiver<MarketEvent>,
    updates: &mut TickStream,
) -> Result<()> {
    let mut requests = updates.take_requests();
    let mut requests_closed = false;
    let tools = view.tools();
    let mut running_calls = FuturesUnordered::new();
    let mut state = ViewState::default();
    let mut viewport = ScrollViewport::default();
    let mut market_open = true;
    let mut frames = tokio::time::interval(FRAME_INTERVAL);
    frames.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut dirty = true;
    let mut configured_refresh = view.refresh_interval();
    let refresh_interval = configured_refresh.unwrap_or(std::time::Duration::from_secs(86400));
    let mut refreshes = tokio::time::interval_at(
        tokio::time::Instant::now() + refresh_interval,
        refresh_interval,
    );
    refreshes.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    updates.publish(view.content())?;
    loop {
        if view.take_navigation().is_some() {
            state.last_error = Some("Cross-Tick navigation is unavailable in this host.".into());
            dirty = true;
        }
        let next_refresh = view.refresh_interval();
        if next_refresh != configured_refresh {
            configured_refresh = next_refresh;
            if let Some(interval) = configured_refresh {
                refreshes =
                    tokio::time::interval_at(tokio::time::Instant::now() + interval, interval);
                refreshes.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            }
        }
        tokio::select! {
            line = requests.next(), if !requests_closed => match line {
                None => {
                    requests_closed = true;
                    if running_calls.is_empty() { break; }
                }
                Some(line) => {
                    if let Some(call) = updates.dispatch(line) {
                        match accept_tool_call(&tools, api, call, running_calls.len()) {
                            Ok(AcceptedToolCall::Query(running)) => running_calls.push(running),
                            Ok(AcceptedToolCall::Action { id, call }) => {
                                let previous_content = view.content();
                                let applied = view.act(call).await;
                                if applied.is_ok() {
                                    let content = view.content();
                                    if content != previous_content {
                                        updates.publish(content)?;
                                    }
                                    dirty = true;
                                }
                                updates.answer(id, applied.map_err(tool_failure));
                            }
                            Err((id, error)) => updates.answer(id, Err(error)),
                        }
                    }
                }
            },
            finished = running_calls.next(), if !running_calls.is_empty() => {
                if let Some((id, result)) = finished {
                    updates.answer(id, result);
                }
                if requests_closed && running_calls.is_empty() { break; }
            }
            _ = refreshes.tick(), if configured_refresh.is_some() => {
                if state.connection != ConnectionState::Connected {
                    state.connection = ConnectionState::Reconnecting;
                    render(view, terminal, &mut state, &mut viewport, settings)?;
                }
                match refresh_cancellable(view, api, &mut input).await {
                    Ok(None) => break,
                    Ok(Some(())) => { state.received_data(); updates.publish(view.content())?; }
                    Err(error) => state.failed(error)?,
                }
                dirty = true;
            }
            _ = frames.tick() => {
                if dirty { render(view, terminal, &mut state, &mut viewport, settings)?; dirty = false; }
            }
            event = market.recv(), if market_open => {
                dirty = true;
                match event {
                    Some(event) => {
                        match event {
                            MarketEvent::Failure { code, message } => state.failed(Error { code, message })?,
                            MarketEvent::Connection(connection) => {
                                let recovered = connection == ConnectionState::Connected && state.connection != ConnectionState::Connected;
                                if recovered && view.refresh_on_recovery() {
                                    state.connection = ConnectionState::Reconnecting;
                                    render(view, terminal, &mut state, &mut viewport, settings)?;
                                    match refresh_cancellable(view, api, &mut input).await {
                                        Ok(None) => break,
                                        Ok(Some(())) => { state.received_data(); updates.publish(view.content())?; }
                                        Err(error) => state.failed(error)?,
                                    }
                                } else {
                                    state.connection = connection;
                                    if connection == ConnectionState::Connected { state.received_data(); }
                                }
                            }
                            event => {
                                let previous_content = view.content();
                                view.on_market(&event);
                                let content = view.content();
                                if content != previous_content {
                                    state.received_data();
                                    updates.publish(content)?;
                                }
                            }
                        }
                    }
                    None => { market_open = false; state.connection = ConnectionState::Disconnected; }
                }
            }
            event = input.next() => {
                dirty = true;
                match event {
                    None => break,
                    Some(Err(error)) => return Err(error.into()),
                    Some(Ok(Event::Resize(_, _))) => { terminal.clear().map_err(|error| Error::usage(error.to_string()))?; }
                    Some(Ok(Event::Key(key))) if key.kind != KeyEventKind::Release => {
                        let capturing = view.captures_text();
                        let interrupted = key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL);
                        if interrupted && capturing { view.cancel_text(); continue; }
                        if interrupted { break; }
                        if !capturing && key.code == KeyCode::Char('q') { break; }
                        if !capturing && key.code == KeyCode::Char('?') { state.help = !state.help; continue; }
                        if state.help { if key.code == KeyCode::Backspace { state.help = false; } continue; }
                        if !capturing && let KeyCode::Char(character) = key.code && viewport.handle_key(character) { continue; }
                        let code = if key.modifiers.contains(KeyModifiers::CONTROL) {
                            match key.code {
                                KeyCode::Char('n') => KeyCode::Down,
                                KeyCode::Char('p') => KeyCode::Up,
                                _ => continue,
                            }
                        } else if key.modifiers.intersects(KeyModifiers::ALT | KeyModifiers::SUPER) {
                            continue;
                        } else { key.code };
                        let previous_content = view.content();
                        // Enter submits the edit, so what it waits for is no longer editing and q / Ctrl+C quit again.
                        let editing = capturing && code != KeyCode::Enter;
                        let handled = match cancellable_unless_editing(view.on_key(code, api, &mut state), &mut input, editing).await {
                            Ok(Some(handled)) => handled,
                            Ok(None) => break,
                            Err(error) => { state.failed(error)?; continue; }
                        };
                        if handled {
                            let content = view.content();
                            if content != previous_content {
                                state.received_data();
                                updates.publish(content)?;
                            }
                            continue;
                        }
                        let length = view.table().rows.len();
                        let page = terminal.size().map_err(|error| Error::usage(error.to_string()))?.height.saturating_sub(4).max(1) as usize;
                        match code {
                            KeyCode::Char('j') | KeyCode::Down => state.selected = (state.selected + 1).min(length.saturating_sub(1)),
                            KeyCode::Char('k') | KeyCode::Up => state.selected = state.selected.saturating_sub(1),
                            KeyCode::PageDown => state.selected = (state.selected + page).min(length.saturating_sub(1)),
                            KeyCode::PageUp => state.selected = state.selected.saturating_sub(page),
                            KeyCode::Char('g') | KeyCode::Home => state.selected = 0,
                            KeyCode::Char('G') | KeyCode::End => state.selected = length.saturating_sub(1),
                            _ => (),
                        }
                    }
                    _ => (),
                }
            }
        }
    }
    if dirty {
        render(view, terminal, &mut state, &mut viewport, settings)?;
    }
    Ok(())
}

fn render<V: LiveView, B: Backend>(
    view: &V,
    terminal: &mut Terminal<B>,
    state: &mut ViewState,
    viewport: &mut ScrollViewport,
    settings: &Settings,
) -> Result<()> {
    let size = terminal
        .size()
        .map_err(|error| Error::usage(error.to_string()))?;
    let minimum = view.minimum_panel_size();
    viewport.set_layout(ViewportLayout::of(
        Rect::new(0, 0, size.width, size.height),
        Size::new(minimum.columns, minimum.rows),
    ));
    let canvas = match viewport.layout() {
        Some(layout) => {
            // ratatui hands out a Frame only through a Terminal, so the canvas is an in-memory one.
            let Ok(mut canvas) = Terminal::new(CanvasBackend::new(layout.canvas));
            let Ok(_) = canvas.draw(|frame| draw_canvas(view, frame, state, settings));
            let backend = canvas.backend();
            let cursor = backend.cursor();
            viewport.follow(
                cursor.map(|cursor| cursor.x).or(state.focus_column),
                cursor.map(|cursor| cursor.y).or(state.focus_row),
            );
            Some((backend.buffer().clone(), cursor))
        }
        None => None,
    };
    terminal
        .draw(|frame| {
            let styles = settings.styles();
            match &canvas {
                None => draw_canvas(view, frame, state, settings),
                Some((buffer, cursor)) => {
                    let area = frame.area();
                    frame.buffer_mut().set_style(area, styles.canvas);
                    viewport.render(buffer, frame.buffer_mut(), styles, settings.ambiguous_width);
                    if let Some(cursor) = cursor.and_then(|cursor| viewport.on_panel(cursor)) {
                        frame.set_cursor_position(cursor);
                    }
                }
            }
            if state.help {
                let area = frame.area();
                let help = Paragraph::new(match viewport.help() {
                    Some(scroll_keys) => format!("{}\n{scroll_keys}", view.help()),
                    None => view.help().to_owned(),
                })
                .block(
                    Block::default()
                        .title(" Keys · ? / Backspace close ")
                        .borders(Borders::ALL)
                        .border_style(styles.border),
                )
                .wrap(Wrap { trim: false })
                .style(styles.overlay_base());
                // A narrow Panel grows the overlay toward its edges before wrapping the keys.
                let width = (area.width * 3 / 4).max(
                    u16::try_from(help.line_width())
                        .unwrap_or(u16::MAX)
                        .min(area.width.saturating_sub(2)),
                );
                let height = (area.height * 3 / 4).max(
                    u16::try_from(help.line_count(width))
                        .unwrap_or(u16::MAX)
                        .min(area.height.saturating_sub(2)),
                );
                let popup = Rect::new(
                    area.x + (area.width / 8).min((area.width - width) / 2),
                    area.y + (area.height / 8).min((area.height - height) / 2),
                    width,
                    height,
                );
                frame.render_widget(Clear, popup);
                frame.buffer_mut().set_style(popup, styles.overlay_base());
                frame.render_widget(help, popup);
            }
        })
        .map_err(|error| Error::usage(error.to_string()))?;
    Ok(())
}

/// Draws the view with the demo heading and the status row into the frame, which is the Panel
/// itself or, for a Panel smaller than the Tick's minimum, the canvas it scrolls over.
fn draw_canvas<V: LiveView>(
    view: &V,
    frame: &mut Frame<'_>,
    state: &mut ViewState,
    settings: &Settings,
) {
    let area = frame.area();
    let styles = settings.styles();
    frame.buffer_mut().set_style(area, styles.canvas);
    state.focus_column = None;
    state.focus_row = None;
    view.draw(frame, state, settings);
    let horizontal_padding = if area.width >= WIDE_PANEL_COLUMNS {
        2
    } else {
        1
    };
    if settings.is_demo {
        let heading_area = Rect::new(area.x, area.y, area.width, 1);
        frame
            .buffer_mut()
            .set_style(heading_area, styles.panel_base());
        write_text(
            frame.buffer_mut(),
            area.x,
            area.y,
            &" ".repeat(area.width as usize),
            area.width as usize,
            settings.ambiguous_width,
            styles.panel_base(),
        );
        let title_width = area
            .width
            .saturating_sub(horizontal_padding * 2)
            .saturating_sub(7);
        write_text(
            frame.buffer_mut(),
            area.x + horizontal_padding,
            area.y,
            &pad(
                &view.title(),
                title_width as usize,
                settings.ambiguous_width,
                false,
            ),
            title_width as usize,
            settings.ambiguous_width,
            styles.panel_base().patch(styles.title),
        );
        write_text(
            frame.buffer_mut(),
            area.right() - horizontal_padding - 4,
            area.y,
            "DEMO",
            4,
            settings.ambiguous_width,
            styles.panel_base().patch(styles.muted_text),
        );
    }
    let status_area = Rect::new(area.x, area.bottom() - 1, area.width, 1);
    let mut status_style = styles.status_base().patch(styles.muted_text);
    if state.last_error.is_some() {
        status_style = status_style.patch(styles.error);
    }
    frame
        .buffer_mut()
        .set_style(status_area, styles.status_base());
    let icon = tickloop_ui::icons::Icon::Connection.glyph(settings.icon_theme);
    let status = if let Some(error) = &state.last_error {
        if state.connection == ConnectionState::Connected {
            error.clone()
        } else {
            format!("{icon} {} · {error}", state.availability())
        }
    } else if settings.is_demo {
        "Simulated data".to_owned()
    } else {
        format!("{icon} {}", state.availability())
    };
    let keys = if view.captures_text() && area.width >= 40 {
        "Enter Apply  Ctrl+C Cancel"
    } else if view.captures_text() {
        "Ctrl+C Cancel"
    } else if area.width >= 40 {
        "? Help  q Quit"
    } else {
        "?  q Quit"
    };
    let keys_width = width(keys, settings.ambiguous_width) as u16;
    write_text(
        frame.buffer_mut(),
        area.x,
        area.bottom() - 1,
        &" ".repeat(area.width as usize),
        area.width as usize,
        settings.ambiguous_width,
        styles.status_base(),
    );
    write_text(
        frame.buffer_mut(),
        area.x + horizontal_padding,
        area.bottom() - 1,
        &status,
        area.width
            .saturating_sub(horizontal_padding * 2 + keys_width + 2) as usize,
        settings.ambiguous_width,
        status_style,
    );
    write_text(
        frame.buffer_mut(),
        area.right() - horizontal_padding - keys_width,
        area.bottom() - 1,
        keys,
        keys_width as usize,
        settings.ambiguous_width,
        styles.status_base().patch(styles.muted_text),
    );
}

pub fn table_content_area(area: Rect) -> Rect {
    let horizontal_padding = if area.width >= WIDE_PANEL_COLUMNS {
        2
    } else {
        1
    };
    let header_y = area.y + if area.height >= TALL_PANEL_ROWS { 2 } else { 1 };
    Rect::new(
        area.x + horizontal_padding,
        header_y,
        area.width.saturating_sub(horizontal_padding * 2),
        area.bottom().saturating_sub(header_y + 1),
    )
}

pub fn draw_table(
    frame: &mut Frame<'_>,
    title: &str,
    table: &TableData,
    priority: &[usize],
    state: &mut ViewState,
    settings: &Settings,
) {
    let area = frame.area();
    let styles = settings.styles();
    frame.buffer_mut().set_style(area, styles.panel_base());
    if area.width < 8 || area.height < 4 {
        frame.render_widget(Paragraph::new("Too small").style(styles.muted_text), area);
        return;
    }
    let inner = table_content_area(area);
    write_text(
        frame.buffer_mut(),
        inner.x,
        area.y,
        title,
        inner.width as usize,
        settings.ambiguous_width,
        styles.panel_base().patch(styles.title),
    );
    let rows = table.display_rows(settings);
    let headers: Vec<&str> = table
        .headers
        .iter()
        .map(|header| crate::output::short_header(header))
        .collect();
    let widths: Vec<usize> = headers
        .iter()
        .enumerate()
        .map(|(index, header)| {
            rows.iter()
                .map(|row| width(&row[index].text, settings.ambiguous_width))
                .max()
                .unwrap_or(0)
                .max(width(header, settings.ambiguous_width))
                .clamp(4, 28)
        })
        .collect();
    let mut remaining = inner.width as usize;
    let mut columns = Vec::new();
    for &index in priority {
        let is_empty_error = table.headers.get(index) == Some(&"Error")
            && table
                .rows
                .iter()
                .all(|row| row.get(index).is_none_or(serde_json::Value::is_null));
        let spacing = if columns.is_empty() { 0 } else { 2 };
        if index < widths.len() && !is_empty_error && widths[index] + spacing <= remaining {
            columns.push(index);
            remaining -= widths[index] + spacing;
        }
    }
    if columns.is_empty() {
        columns.push(0);
    }
    columns.sort_unstable();
    let visible = inner.height.saturating_sub(1) as usize;
    if state.selected < state.offset {
        state.offset = state.selected;
    }
    if state.selected >= state.offset + visible {
        state.offset = state.selected.saturating_sub(visible.saturating_sub(1));
    }
    let mut x = inner.x;
    for &column in &columns {
        let text = pad(
            headers[column],
            widths[column],
            settings.ambiguous_width,
            column >= 2,
        );
        write_text(
            frame.buffer_mut(),
            x,
            inner.y,
            &text,
            inner.right().saturating_sub(x) as usize,
            settings.ambiguous_width,
            styles.panel_base().patch(styles.muted_text),
        );
        x = x.saturating_add(widths[column] as u16 + 2);
    }
    for (row_index, row) in rows.iter().enumerate().skip(state.offset).take(visible) {
        let y = inner.y + 1 + (row_index - state.offset) as u16;
        if row_index == state.selected {
            state.focus_row = Some(y);
            frame.buffer_mut().set_style(
                Rect::new(inner.x, y, inner.width, 1),
                styles.panel_base().patch(styles.selection),
            );
            write_text(
                frame.buffer_mut(),
                area.x,
                y,
                ">",
                1,
                settings.ambiguous_width,
                styles.panel_base().patch(styles.selection),
            );
        }
        let mut x = inner.x;
        for &column in &columns {
            let cell = &row[column];
            let text = pad(
                &cell.text,
                widths[column],
                settings.ambiguous_width,
                column >= 2,
            );
            let mut style = styles.panel_base().patch(styles.price(cell.direction));
            if cell.is_error {
                style = style.patch(styles.error);
            }
            if row_index == state.selected {
                style = style.patch(styles.selection);
            }
            write_text(
                frame.buffer_mut(),
                x,
                y,
                &text,
                inner.right().saturating_sub(x) as usize,
                settings.ambiguous_width,
                style,
            );
            x = x.saturating_add(widths[column] as u16 + 2);
        }
    }
    if rows.is_empty() {
        write_text(
            frame.buffer_mut(),
            inner.x,
            inner.y + 1,
            "No data",
            inner.width as usize,
            settings.ambiguous_width,
            styles.panel_base().patch(styles.muted_text),
        );
    }
}
