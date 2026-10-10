use async_trait::async_trait;
use serde_json::{Value, json};
use tick_market::{
    args::{KlineArgs, OnceArgs, OutputFormat},
    commands::kline,
    config::Settings,
    metadata::TickKind,
};
use tick_provider::{
    api::{Candle, CandleAdjust, CandlePeriod, CandleRequest, Quote, QuoteApi},
    error::Result,
};
use tick_sdk::stream::TickStream;

fn candles() -> Vec<Candle> {
    serde_json::from_value(json!([
        {"timestamp":"2026-09-08T00:00:00Z", "open":"100", "high":"110", "low":"95", "close":"105", "volume":100, "turnover":"10200"},
        {"timestamp":"2026-09-09T00:00:00Z", "open":"105", "high":"112", "low":"100", "close":"102", "volume":200, "turnover":"20900"},
        {"timestamp":"2026-09-10T00:00:00Z", "open":"102", "high":"120", "low":"101", "close":"118", "volume":300, "turnover":"33600"}
    ])).unwrap()
}
struct History;
#[async_trait]
impl QuoteApi for History {
    async fn quotes(&self, _: &[String]) -> Result<Vec<Quote>> {
        panic!("Kline fetches candles")
    }
    async fn candles(&self, request: &CandleRequest) -> Result<Vec<Candle>> {
        assert_eq!(request.symbol, "700.HK");
        assert_eq!(request.period, CandlePeriod::Day);
        assert_eq!(request.count, 3);
        assert_eq!(request.adjust, CandleAdjust::Forward);
        Ok(candles())
    }
}
#[tokio::test]
async fn kline_once_outputs_adjusted_candles_without_reapplying_adjustment() {
    let args = KlineArgs {
        symbol: "700".into(),
        period: CandlePeriod::Day,
        count: Some(3),
        adjust: CandleAdjust::Forward,
        event: Default::default(),
        output: OnceArgs {
            once: true,
            format: OutputFormat::Json,
        },
    };
    let mut output = Vec::new();
    let mut updates = TickStream::in_memory(TickKind::Kline);
    kline::run_once(
        &args,
        &History,
        &Settings::default(),
        false,
        &mut output,
        &mut updates,
    )
    .await
    .unwrap();
    let rows: Value = serde_json::from_slice(&output).unwrap();
    assert_eq!(rows.as_array().unwrap().len(), 3);
    assert_eq!(rows[0]["open"], "100");
    assert_eq!(rows[2]["close"], "118");
    assert_eq!(rows[2]["volume"], 300);
    let record: Value = updates.records().all().remove(0);
    assert_eq!(record["data"]["symbol"], "700.HK");
    assert_eq!(record["data"]["period"], "day");
    assert_eq!(record["data"]["adjust"], "forward");
    assert_eq!(record["data"]["candles"], rows);
}

struct LiveHistory {
    events: std::sync::Mutex<
        Option<tokio::sync::mpsc::UnboundedReceiver<tick_provider::api::MarketEvent>>,
    >,
    subscriptions: std::sync::Mutex<Vec<(bool, CandlePeriod)>>,
}
#[async_trait]
impl QuoteApi for LiveHistory {
    async fn quotes(&self, _: &[String]) -> Result<Vec<Quote>> {
        unreachable!()
    }
    async fn candles(&self, _: &CandleRequest) -> Result<Vec<Candle>> {
        Ok(candles())
    }
    async fn subscribe_candles(&self, request: &CandleRequest) -> Result<bool> {
        self.subscriptions
            .lock()
            .unwrap()
            .push((true, request.period));
        Ok(true)
    }
    async fn unsubscribe_candles(&self, _: &str, period: CandlePeriod) -> Result<()> {
        self.subscriptions.lock().unwrap().push((false, period));
        Ok(())
    }
    async fn take_events(
        &self,
    ) -> Result<tokio::sync::mpsc::UnboundedReceiver<tick_provider::api::MarketEvent>> {
        Ok(self.events.lock().unwrap().take().unwrap())
    }
}

#[tokio::test]
async fn kline_cursor_period_and_push_updates_share_one_live_chart() {
    use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
    use tick_provider::api::{CandleUpdate, MarketEvent};
    let (push, events) = tokio::sync::mpsc::unbounded_channel();
    let api = LiveHistory {
        events: std::sync::Mutex::new(Some(events)),
        subscriptions: std::sync::Mutex::new(Vec::new()),
    };
    let (keys, input) = tokio::sync::mpsc::unbounded_channel();
    let producer = tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        for code in [
            KeyCode::Left,
            KeyCode::Right,
            KeyCode::Char('m'),
            KeyCode::Char('5'),
        ] {
            keys.send(Ok(Event::Key(KeyEvent::new(code, KeyModifiers::NONE))))
                .unwrap();
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        let mut candle = candles().pop().unwrap();
        candle.close = "125".parse().unwrap();
        candle.high = "128".parse().unwrap();
        candle.low = "99".parse().unwrap();
        candle.volume = 999;
        let update = CandleUpdate {
            symbol: "700.HK".into(),
            period: CandlePeriod::FiveMinute,
            adjust: CandleAdjust::None,
            candles: vec![candle],
            replace: false,
            requested_at: None,
        };
        push.send(MarketEvent::Candles(update.clone())).unwrap();
        push.send(MarketEvent::Candles(update)).unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        keys.send(Ok(Event::Key(KeyEvent::new(
            KeyCode::Char('q'),
            KeyModifiers::NONE,
        ))))
        .unwrap();
    });
    let args = KlineArgs {
        symbol: "700".into(),
        period: CandlePeriod::Day,
        count: Some(3),
        adjust: CandleAdjust::None,
        event: Default::default(),
        output: OnceArgs::default(),
    };
    let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(100, 24)).unwrap();
    let mut updates = TickStream::in_memory(TickKind::Kline);
    kline::run_live(
        &args,
        &api,
        &Settings::default(),
        &mut terminal,
        tokio_stream::wrappers::UnboundedReceiverStream::new(input),
        &mut updates,
    )
    .await
    .unwrap();
    producer.await.unwrap();
    let screen: String = terminal
        .backend()
        .buffer()
        .content
        .iter()
        .map(|cell| cell.symbol())
        .collect();
    assert!(screen.contains("5m"), "{screen}");
    assert!(screen.contains("C 125"), "{screen}");
    assert!(screen.contains("V 999"), "{screen}");
    assert!(screen.contains("MA5/10/20"), "{screen}");
    let records: Vec<Value> = updates.records().all();
    assert_eq!(
        records.len(),
        3,
        "snapshot, new period, changed candle; cursor and overlay are UI state"
    );
    assert_eq!(records[2]["data"]["candles"][2]["close"], "125");
    assert_eq!(
        *api.subscriptions.lock().unwrap(),
        vec![
            (true, CandlePeriod::Day),
            (true, CandlePeriod::FiveMinute),
            (false, CandlePeriod::Day),
            (false, CandlePeriod::FiveMinute)
        ]
    );
}

struct PollingHistory {
    calls: std::sync::atomic::AtomicUsize,
    events: std::sync::Mutex<
        Option<tokio::sync::mpsc::UnboundedReceiver<tick_provider::api::MarketEvent>>,
    >,
}
#[async_trait]
impl QuoteApi for PollingHistory {
    async fn quotes(&self, _: &[String]) -> Result<Vec<Quote>> {
        unreachable!()
    }
    async fn candles(&self, _: &CandleRequest) -> Result<Vec<Candle>> {
        let calls = self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let mut history = candles();
        if calls > 0 {
            history.last_mut().unwrap().close = "119".parse().unwrap();
        }
        Ok(history)
    }
    async fn take_events(
        &self,
    ) -> Result<tokio::sync::mpsc::UnboundedReceiver<tick_provider::api::MarketEvent>> {
        Ok(self.events.lock().unwrap().take().unwrap())
    }
}
#[tokio::test(start_paused = true)]
async fn periods_without_push_refresh_on_the_documented_interval() {
    use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
    let (_push, events) = tokio::sync::mpsc::unbounded_channel();
    let api = PollingHistory {
        calls: std::sync::atomic::AtomicUsize::new(0),
        events: std::sync::Mutex::new(Some(events)),
    };
    let (keys, input) = tokio::sync::mpsc::unbounded_channel();
    let producer = tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_secs(31)).await;
        keys.send(Ok(Event::Key(KeyEvent::new(
            KeyCode::Char('q'),
            KeyModifiers::NONE,
        ))))
        .unwrap();
    });
    let args = KlineArgs {
        symbol: "700".into(),
        period: CandlePeriod::Day,
        count: Some(3),
        adjust: CandleAdjust::None,
        event: Default::default(),
        output: OnceArgs::default(),
    };
    let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 20)).unwrap();
    let mut updates = TickStream::in_memory(TickKind::Kline);
    kline::run_live(
        &args,
        &api,
        &Settings::default(),
        &mut terminal,
        tokio_stream::wrappers::UnboundedReceiverStream::new(input),
        &mut updates,
    )
    .await
    .unwrap();
    producer.await.unwrap();
    assert_eq!(api.calls.load(std::sync::atomic::Ordering::SeqCst), 2);
    let records: Vec<Value> = updates.records().all();
    assert_eq!(records.len(), 2);
    assert_eq!(records[1]["data"]["candles"][2]["close"], "119");
    let screen: String = terminal
        .backend()
        .buffer()
        .content
        .iter()
        .map(|cell| cell.symbol())
        .collect();
    assert!(screen.contains("30s refresh"), "{screen}");
}

#[tokio::test]
async fn forward_adjusted_history_accepts_only_complete_adjusted_updates() {
    use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
    use tick_provider::api::{CandleUpdate, MarketEvent};
    let (push, events) = tokio::sync::mpsc::unbounded_channel();
    let api = LiveHistory {
        events: std::sync::Mutex::new(Some(events)),
        subscriptions: std::sync::Mutex::new(Vec::new()),
    };
    let (keys, input) = tokio::sync::mpsc::unbounded_channel();
    let producer = tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        let mut raw = candles();
        raw[2].close = "200".parse().unwrap();
        raw[2].high = "200".parse().unwrap();
        push.send(MarketEvent::Candles(CandleUpdate {
            symbol: "700.HK".into(),
            period: CandlePeriod::Day,
            adjust: CandleAdjust::None,
            candles: raw.clone(),
            replace: false,
            requested_at: None,
        }))
        .unwrap();
        push.send(MarketEvent::Candles(CandleUpdate {
            symbol: "700.HK".into(),
            period: CandlePeriod::Day,
            adjust: CandleAdjust::Forward,
            candles: raw,
            replace: false,
            requested_at: None,
        }))
        .unwrap();
        let mut adjusted = candles();
        adjusted[0].open = "96".parse().unwrap();
        adjusted[2].close = "119".parse().unwrap();
        push.send(MarketEvent::Candles(CandleUpdate {
            symbol: "700.HK".into(),
            period: CandlePeriod::Day,
            adjust: CandleAdjust::Forward,
            candles: adjusted,
            replace: true,
            requested_at: None,
        }))
        .unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        keys.send(Ok(Event::Key(KeyEvent::new(
            KeyCode::Char('q'),
            KeyModifiers::NONE,
        ))))
        .unwrap();
    });
    let args = KlineArgs {
        symbol: "700.HK".into(),
        period: CandlePeriod::Day,
        count: Some(3),
        adjust: CandleAdjust::Forward,
        event: Default::default(),
        output: OnceArgs::default(),
    };
    let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 20)).unwrap();
    let mut updates = TickStream::in_memory(TickKind::Kline);
    kline::run_live(
        &args,
        &api,
        &Settings::default(),
        &mut terminal,
        tokio_stream::wrappers::UnboundedReceiverStream::new(input),
        &mut updates,
    )
    .await
    .unwrap();
    producer.await.unwrap();
    let records: Vec<Value> = updates.records().all();
    assert_eq!(records.len(), 2);
    assert_eq!(records[1]["data"]["candles"][0]["open"], "96");
    assert_eq!(records[1]["data"]["candles"][2]["close"], "119");
    assert_eq!(
        records[1]["data"]["update_source"],
        "server_adjusted_snapshot"
    );
}

#[derive(Clone)]
struct ResizableBackend(std::sync::Arc<std::sync::Mutex<ratatui::backend::TestBackend>>);
impl ratatui::backend::Backend for ResizableBackend {
    type Error = std::convert::Infallible;
    fn draw<'a, I>(&mut self, content: I) -> std::result::Result<(), Self::Error>
    where
        I: Iterator<Item = (u16, u16, &'a ratatui::buffer::Cell)>,
    {
        self.0.lock().unwrap().draw(content)
    }
    fn hide_cursor(&mut self) -> std::result::Result<(), Self::Error> {
        self.0.lock().unwrap().hide_cursor()
    }
    fn show_cursor(&mut self) -> std::result::Result<(), Self::Error> {
        self.0.lock().unwrap().show_cursor()
    }
    fn get_cursor_position(
        &mut self,
    ) -> std::result::Result<ratatui::layout::Position, Self::Error> {
        self.0.lock().unwrap().get_cursor_position()
    }
    fn set_cursor_position<P: Into<ratatui::layout::Position>>(
        &mut self,
        position: P,
    ) -> std::result::Result<(), Self::Error> {
        self.0.lock().unwrap().set_cursor_position(position)
    }
    fn clear(&mut self) -> std::result::Result<(), Self::Error> {
        self.0.lock().unwrap().clear()
    }
    fn clear_region(
        &mut self,
        region: ratatui::backend::ClearType,
    ) -> std::result::Result<(), Self::Error> {
        self.0.lock().unwrap().clear_region(region)
    }
    fn size(&self) -> std::result::Result<ratatui::layout::Size, Self::Error> {
        self.0.lock().unwrap().size()
    }
    fn window_size(&mut self) -> std::result::Result<ratatui::backend::WindowSize, Self::Error> {
        self.0.lock().unwrap().window_size()
    }
    fn flush(&mut self) -> std::result::Result<(), Self::Error> {
        self.0.lock().unwrap().flush()
    }
}

struct LongHistory {
    events: std::sync::Mutex<
        Option<tokio::sync::mpsc::UnboundedReceiver<tick_provider::api::MarketEvent>>,
    >,
}
#[async_trait]
impl QuoteApi for LongHistory {
    async fn quotes(&self, _: &[String]) -> Result<Vec<Quote>> {
        unreachable!()
    }
    async fn candles(&self, _: &CandleRequest) -> Result<Vec<Candle>> {
        let base = candles()[0].clone();
        Ok((0..80)
            .map(|index| Candle {
                timestamp: base.timestamp + time::Duration::days(index),
                open: (100 + index).into(),
                high: (110 + index).into(),
                low: (95 + index).into(),
                close: (105 + index).into(),
                volume: 100 + index,
                turnover: 1000.into(),
            })
            .collect())
    }
    async fn subscribe_candles(&self, _: &CandleRequest) -> Result<bool> {
        Ok(true)
    }
    async fn take_events(
        &self,
    ) -> Result<tokio::sync::mpsc::UnboundedReceiver<tick_provider::api::MarketEvent>> {
        Ok(self.events.lock().unwrap().take().unwrap())
    }
}
#[tokio::test]
async fn resizing_recomputes_visible_candles_without_losing_selection_or_emitting_ticks() {
    use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
    let (_push, events) = tokio::sync::mpsc::unbounded_channel();
    let api = LongHistory {
        events: std::sync::Mutex::new(Some(events)),
    };
    let backend = ResizableBackend(std::sync::Arc::new(std::sync::Mutex::new(
        ratatui::backend::TestBackend::new(110, 24),
    )));
    let observed = backend.0.clone();
    let (keys, input) = tokio::sync::mpsc::unbounded_channel();
    let producer = tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        for _ in 0..5 {
            keys.send(Ok(Event::Key(KeyEvent::new(
                KeyCode::Left,
                KeyModifiers::NONE,
            ))))
            .unwrap();
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let mut samples = Vec::new();
        for (width, height) in [(45, 14), (20, 8), (110, 24)] {
            observed.lock().unwrap().resize(width, height);
            keys.send(Ok(Event::Resize(width, height))).unwrap();
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            let buffer = observed.lock().unwrap().buffer().clone();
            samples.push((width, buffer));
        }
        keys.send(Ok(Event::Key(KeyEvent::new(
            KeyCode::Char('q'),
            KeyModifiers::NONE,
        ))))
        .unwrap();
        samples
    });
    let args = KlineArgs {
        symbol: "700.HK".into(),
        period: CandlePeriod::Day,
        count: Some(80),
        adjust: CandleAdjust::None,
        event: Default::default(),
        output: OnceArgs::default(),
    };
    let mut terminal = ratatui::Terminal::new(backend).unwrap();
    let mut updates = TickStream::in_memory(TickKind::Kline);
    kline::run_live(
        &args,
        &api,
        &Settings::default(),
        &mut terminal,
        tokio_stream::wrappers::UnboundedReceiverStream::new(input),
        &mut updates,
    )
    .await
    .unwrap();
    let samples = producer.await.unwrap();
    let mut plotted = Vec::new();
    for (width, buffer) in samples {
        let text: String = buffer.content.iter().map(|cell| cell.symbol()).collect();
        if width == 20 {
            assert!(text.contains("━") && text.contains("↕ 1–7/14"), "{text}");
        } else {
            assert!(text.contains("C 179"), "selection survives resize: {text}");
            plotted.push(
                buffer
                    .content
                    .iter()
                    .filter(|cell| {
                        matches!(cell.symbol(), "▁" | "▂" | "▃" | "▄" | "▅" | "▆" | "▇" | "█")
                    })
                    .count(),
            );
        }
    }
    assert!(
        plotted[1] > plotted[0],
        "wider viewport fills more candles: {plotted:?}"
    );
    assert_eq!(updates.records().len(), 1);
}

#[tokio::test]
async fn kline_uses_injected_renderer_styles_for_its_entire_panel_and_chart() {
    use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
    use ratatui::style::{Color, Modifier, Style};
    let (_push, events) = tokio::sync::mpsc::unbounded_channel();
    let api = LongHistory {
        events: std::sync::Mutex::new(Some(events)),
    };
    let mut styles = Settings::default().styles();
    styles.canvas = Style::new().bg(Color::Rgb(1, 2, 3));
    styles.panel = Style::new().bg(Color::Rgb(4, 5, 6));
    styles.text = Style::new().fg(Color::Rgb(7, 8, 9));
    styles.border = Style::new()
        .fg(Color::Rgb(11, 12, 13))
        .add_modifier(Modifier::ITALIC);
    styles.title = Style::new().fg(Color::Rgb(21, 22, 23));
    styles.muted_text = Style::new().fg(Color::Rgb(31, 32, 33));
    styles.price_up = Style::new()
        .fg(Color::Rgb(41, 42, 43))
        .add_modifier(Modifier::UNDERLINED);
    styles.price_down = Style::new().fg(Color::Rgb(51, 52, 53));
    styles.moving_averages = [
        Style::new().fg(Color::Rgb(61, 62, 63)),
        Style::new().fg(Color::Rgb(71, 72, 73)),
        Style::new().fg(Color::Rgb(81, 82, 83)),
    ];
    styles.chart_cursor = Style::new()
        .bg(Color::Rgb(91, 92, 93))
        .add_modifier(Modifier::REVERSED);
    styles.status_bar = Style::new().bg(Color::Rgb(101, 102, 103));
    let settings = Settings {
        renderer_styles: Some(styles),
        ..Settings::default()
    };
    let input = futures_util::stream::iter(
        [KeyCode::Char('m'), KeyCode::Char('q')]
            .map(|code| Ok(Event::Key(KeyEvent::new(code, KeyModifiers::NONE)))),
    );
    let args = KlineArgs {
        symbol: "700.HK".into(),
        period: CandlePeriod::Day,
        count: Some(80),
        adjust: CandleAdjust::None,
        event: Default::default(),
        output: OnceArgs::default(),
    };
    let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(110, 24)).unwrap();
    kline::run_live(
        &args,
        &api,
        &settings,
        &mut terminal,
        input,
        &mut TickStream::in_memory(TickKind::Kline),
    )
    .await
    .unwrap();
    let buffer = terminal.backend().buffer();
    assert_eq!(buffer[(0, 0)].fg, Color::Rgb(11, 12, 13));
    assert!(buffer[(0, 0)].modifier.contains(Modifier::ITALIC));
    assert_eq!(
        buffer[(2, 0)].fg,
        Color::Rgb(11, 12, 13),
        "border caption inherits the border style, not the Title role"
    );
    assert_eq!(
        buffer[(108, 21)].bg,
        Color::Rgb(4, 5, 6),
        "unoccupied panel cells retain the injected background"
    );
    assert_eq!(
        buffer[(108, 22)].bg,
        Color::Rgb(101, 102, 103),
        "status row includes its trailing blank cells"
    );
    assert!(buffer.content.iter().any(|cell| cell.symbol() == "▲"
        && cell.bg == Color::Rgb(91, 92, 93)
        && cell.modifier.contains(Modifier::REVERSED)));
    assert!(buffer.content.iter().any(|cell| cell.symbol() == "┃"
        && cell.fg == Color::Rgb(41, 42, 43)
        && cell.modifier.contains(Modifier::UNDERLINED)));
    for color in [
        Color::Rgb(61, 62, 63),
        Color::Rgb(71, 72, 73),
        Color::Rgb(81, 82, 83),
    ] {
        assert!(
            buffer.content.iter().any(|cell| cell.fg == color
                && cell
                    .symbol()
                    .chars()
                    .any(|glyph| ('\u{2801}'..='\u{28ff}').contains(&glyph))),
            "each moving average uses the injected style {color:?}"
        );
    }
}

struct SyncHistory {
    calls: std::sync::atomic::AtomicUsize,
    events: std::sync::Mutex<
        Option<tokio::sync::mpsc::UnboundedReceiver<tick_provider::api::MarketEvent>>,
    >,
}
#[async_trait]
impl QuoteApi for SyncHistory {
    async fn quotes(&self, _: &[String]) -> Result<Vec<Quote>> {
        unreachable!()
    }
    async fn candles(&self, _: &CandleRequest) -> Result<Vec<Candle>> {
        let mut history = candles();
        if self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0 {
            history.remove(1);
        } else {
            let last = history.last_mut().unwrap();
            last.high = 130.into();
            last.low = 98.into();
            last.close = 125.into();
            last.volume = 500;
            last.turnover = 60000.into();
        }
        Ok(history)
    }
    async fn subscribe_candles(&self, _: &CandleRequest) -> Result<bool> {
        Ok(true)
    }
    async fn take_events(
        &self,
    ) -> Result<tokio::sync::mpsc::UnboundedReceiver<tick_provider::api::MarketEvent>> {
        Ok(self.events.lock().unwrap().take().unwrap())
    }
}
#[tokio::test(start_paused = true)]
async fn pushed_periods_sync_missing_history_and_reject_regressive_raw_candles() {
    use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
    use tick_provider::api::{CandleUpdate, MarketEvent};
    let (push, events) = tokio::sync::mpsc::unbounded_channel();
    let api = SyncHistory {
        calls: std::sync::atomic::AtomicUsize::new(0),
        events: std::sync::Mutex::new(Some(events)),
    };
    let (keys, input) = tokio::sync::mpsc::unbounded_channel();
    let producer = tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_secs(31)).await;
        push.send(MarketEvent::Candles(CandleUpdate {
            symbol: "700.HK".into(),
            period: CandlePeriod::FiveMinute,
            adjust: CandleAdjust::None,
            candles: vec![candles().pop().unwrap()],
            replace: false,
            requested_at: None,
        }))
        .unwrap();
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        keys.send(Ok(Event::Key(KeyEvent::new(
            KeyCode::Char('q'),
            KeyModifiers::NONE,
        ))))
        .unwrap();
    });
    let args = KlineArgs {
        symbol: "700.HK".into(),
        period: CandlePeriod::FiveMinute,
        count: Some(3),
        adjust: CandleAdjust::None,
        event: Default::default(),
        output: OnceArgs::default(),
    };
    let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(100, 24)).unwrap();
    let mut updates = TickStream::in_memory(TickKind::Kline);
    kline::run_live(
        &args,
        &api,
        &Settings::default(),
        &mut terminal,
        tokio_stream::wrappers::UnboundedReceiverStream::new(input),
        &mut updates,
    )
    .await
    .unwrap();
    producer.await.unwrap();
    assert_eq!(api.calls.load(std::sync::atomic::Ordering::SeqCst), 2);
    let records: Vec<Value> = updates.records().all();
    assert_eq!(
        records.len(),
        2,
        "initial history and completed history only"
    );
    let complete = &records[1]["data"];
    assert_eq!(complete["update_source"], "server_snapshot");
    assert_eq!(complete["candles"].as_array().unwrap().len(), 3);
    assert_eq!(complete["candles"][2]["high"], "130");
    assert_eq!(complete["candles"][2]["low"], "98");
    assert_eq!(complete["candles"][2]["volume"], 500);
    assert_eq!(complete["candles"][2]["turnover"], "60000");
    let screen: String = terminal
        .backend()
        .buffer()
        .content
        .iter()
        .map(|cell| cell.symbol())
        .collect();
    assert!(
        screen.contains("C 125") && screen.contains("V 500"),
        "{screen}"
    );
}

#[derive(Clone, Copy)]
enum SwitchFailure {
    SubscribeNew,
    RetireOld,
}
struct FaultySwitchHistory {
    failure: SwitchFailure,
    failed: std::sync::atomic::AtomicBool,
    active: std::sync::Mutex<std::collections::BTreeSet<CandlePeriod>>,
    calls: std::sync::Mutex<Vec<(bool, CandlePeriod)>>,
    events: std::sync::Mutex<
        Option<tokio::sync::mpsc::UnboundedReceiver<tick_provider::api::MarketEvent>>,
    >,
}
#[async_trait]
impl QuoteApi for FaultySwitchHistory {
    async fn quotes(&self, _: &[String]) -> Result<Vec<Quote>> {
        unreachable!()
    }
    async fn candles(&self, _: &CandleRequest) -> Result<Vec<Candle>> {
        Ok(candles())
    }
    async fn subscribe_candles(&self, request: &CandleRequest) -> Result<bool> {
        self.calls.lock().unwrap().push((true, request.period));
        self.active.lock().unwrap().insert(request.period);
        if matches!(self.failure, SwitchFailure::SubscribeNew)
            && request.period == CandlePeriod::FiveMinute
            && !self.failed.swap(true, std::sync::atomic::Ordering::SeqCst)
        {
            return Err(tick_provider::error::Error::network(
                "new subscription response lost",
            ));
        }
        Ok(true)
    }
    async fn unsubscribe_candles(&self, _: &str, period: CandlePeriod) -> Result<()> {
        self.calls.lock().unwrap().push((false, period));
        self.active.lock().unwrap().remove(&period);
        if matches!(self.failure, SwitchFailure::RetireOld)
            && period == CandlePeriod::Day
            && !self.failed.swap(true, std::sync::atomic::Ordering::SeqCst)
        {
            return Err(tick_provider::error::Error::network(
                "old unsubscribe response lost",
            ));
        }
        Ok(())
    }
    async fn take_events(
        &self,
    ) -> Result<tokio::sync::mpsc::UnboundedReceiver<tick_provider::api::MarketEvent>> {
        Ok(self.events.lock().unwrap().take().unwrap())
    }
}
async fn assert_failed_switch_keeps_original_period(failure: SwitchFailure) {
    use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
    use tick_provider::api::{CandleUpdate, MarketEvent};
    let (push, events) = tokio::sync::mpsc::unbounded_channel();
    let api = std::sync::Arc::new(FaultySwitchHistory {
        failure,
        failed: std::sync::atomic::AtomicBool::new(false),
        active: std::sync::Mutex::new(Default::default()),
        calls: std::sync::Mutex::new(Vec::new()),
        events: std::sync::Mutex::new(Some(events)),
    });
    let observed = api.clone();
    let (keys, input) = tokio::sync::mpsc::unbounded_channel();
    let producer = tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        keys.send(Ok(Event::Key(KeyEvent::new(
            KeyCode::Char('5'),
            KeyModifiers::NONE,
        ))))
        .unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        let active = observed.active.lock().unwrap().clone();
        if active.contains(&CandlePeriod::Day) {
            let mut last = candles().pop().unwrap();
            last.close = 125.into();
            last.high = 130.into();
            last.volume = 400;
            push.send(MarketEvent::Candles(CandleUpdate {
                symbol: "700.HK".into(),
                period: CandlePeriod::Day,
                adjust: CandleAdjust::None,
                candles: vec![last],
                replace: false,
                requested_at: None,
            }))
            .unwrap();
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        keys.send(Ok(Event::Key(KeyEvent::new(
            KeyCode::Char('q'),
            KeyModifiers::NONE,
        ))))
        .unwrap();
        active
    });
    let args = KlineArgs {
        symbol: "700.HK".into(),
        period: CandlePeriod::Day,
        count: Some(3),
        adjust: CandleAdjust::None,
        event: Default::default(),
        output: OnceArgs::default(),
    };
    let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(100, 24)).unwrap();
    let mut updates = TickStream::in_memory(TickKind::Kline);
    kline::run_live(
        &args,
        api.as_ref(),
        &Settings::default(),
        &mut terminal,
        tokio_stream::wrappers::UnboundedReceiverStream::new(input),
        &mut updates,
    )
    .await
    .unwrap();
    assert_eq!(
        producer.await.unwrap(),
        [CandlePeriod::Day].into_iter().collect(),
        "failed switch restores the original subscription and removes the new one"
    );
    assert!(
        api.active.lock().unwrap().is_empty(),
        "exit removes remaining subscriptions"
    );
    let records: Vec<Value> = updates.records().all();
    assert_eq!(
        records.len(),
        2,
        "original view continues receiving updates"
    );
    assert!(
        records
            .iter()
            .all(|record| record["data"]["period"] == "day")
    );
    assert_eq!(records[1]["data"]["candles"][2]["close"], "125");
}
#[tokio::test(start_paused = true)]
async fn failed_new_period_subscription_is_rolled_back() {
    assert_failed_switch_keeps_original_period(SwitchFailure::SubscribeNew).await;
}
#[tokio::test(start_paused = true)]
async fn failed_old_period_retirement_restores_original_subscription() {
    assert_failed_switch_keeps_original_period(SwitchFailure::RetireOld).await;
}

#[tokio::test(start_paused = true)]
async fn delayed_server_snapshot_cannot_overwrite_a_newer_history_refresh() {
    use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
    use tick_provider::api::{CandleUpdate, MarketEvent};
    let (push, events) = tokio::sync::mpsc::unbounded_channel();
    let api = SyncHistory {
        calls: std::sync::atomic::AtomicUsize::new(0),
        events: std::sync::Mutex::new(Some(events)),
    };
    let (keys, input) = tokio::sync::mpsc::unbounded_channel();
    let producer = tokio::spawn(async move {
        let requested_at = tokio::time::Instant::now();
        tokio::time::sleep(std::time::Duration::from_secs(31)).await;
        push.send(MarketEvent::Candles(CandleUpdate {
            symbol: "700.HK".into(),
            period: CandlePeriod::FiveMinute,
            adjust: CandleAdjust::None,
            candles: candles(),
            replace: true,
            requested_at: Some(requested_at),
        }))
        .unwrap();
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        keys.send(Ok(Event::Key(KeyEvent::new(
            KeyCode::Char('q'),
            KeyModifiers::NONE,
        ))))
        .unwrap();
    });
    let args = KlineArgs {
        symbol: "700.HK".into(),
        period: CandlePeriod::FiveMinute,
        count: Some(3),
        adjust: CandleAdjust::None,
        event: Default::default(),
        output: OnceArgs::default(),
    };
    let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(100, 24)).unwrap();
    let mut updates = TickStream::in_memory(TickKind::Kline);
    kline::run_live(
        &args,
        &api,
        &Settings::default(),
        &mut terminal,
        tokio_stream::wrappers::UnboundedReceiverStream::new(input),
        &mut updates,
    )
    .await
    .unwrap();
    producer.await.unwrap();
    assert_eq!(api.calls.load(std::sync::atomic::Ordering::SeqCst), 2);
    let records: Vec<Value> = updates.records().all();
    assert_eq!(
        records.len(),
        2,
        "the delayed earlier request must not publish a rollback"
    );
    assert_eq!(records[1]["data"]["candles"][2]["volume"], 500);
    assert!(records[1]["data"].get("requested_at").is_none());
    let screen: String = terminal
        .backend()
        .buffer()
        .content
        .iter()
        .map(|cell| cell.symbol())
        .collect();
    assert!(
        screen.contains("C 125") && screen.contains("V 500"),
        "{screen}"
    );
}

const MARK_COLOR: ratatui::style::Color = ratatui::style::Color::Rgb(201, 202, 203);

async fn marked_view() -> kline::KlineView {
    use tick_market::runtime::LiveView;
    use tick_sdk::tools::ToolCall;
    let mut view = kline::KlineView::new(
        CandleRequest {
            symbol: "700.HK".into(),
            period: CandlePeriod::Day,
            count: 3,
            adjust: CandleAdjust::None,
        },
        candles(),
        false,
    );
    for (label, price) in [("moon", 500.0), ("target", 118.0), ("near", 117.6)] {
        view.act(ToolCall {
            tool: "mark".into(),
            args: json!({"label": label, "price": price}),
        })
        .await
        .unwrap();
    }
    view
}

fn draw_marked(view: &kline::KlineView, columns: u16, rows: u16) -> ratatui::buffer::Buffer {
    use tick_market::runtime::{LiveView, ViewState};
    let mut styles = Settings::default().styles();
    styles.price_mark = ratatui::style::Style::new().fg(MARK_COLOR);
    let settings = Settings {
        renderer_styles: Some(styles),
        ..Settings::default()
    };
    let mut terminal =
        ratatui::Terminal::new(ratatui::backend::TestBackend::new(columns, rows)).unwrap();
    let mut state = ViewState::default();
    terminal
        .draw(|frame| view.draw(frame, &mut state, &settings))
        .unwrap();
    terminal.backend().buffer().clone()
}

fn row_text(buffer: &ratatui::buffer::Buffer, row: u16) -> String {
    (0..buffer.area.width)
        .map(|column| buffer[(column, row)].symbol())
        .collect()
}

fn mark_rows(buffer: &ratatui::buffer::Buffer) -> Vec<u16> {
    (0..buffer.area.height)
        .filter(|row| (0..buffer.area.width).any(|column| buffer[(column, *row)].fg == MARK_COLOR))
        .collect()
}

#[tokio::test]
async fn marks_draw_labelled_lines_that_stack_and_stay_inside_the_chart() {
    let view = marked_view().await;
    let buffer = draw_marked(&view, 110, 24);

    let lines: Vec<(u16, u16)> = (0..buffer.area.height)
        .flat_map(|row| (0..buffer.area.width).map(move |column| (column, row)))
        .filter(|&(column, row)| buffer[(column, row)].symbol() == "\u{254c}")
        .collect();
    assert!(!lines.is_empty(), "no mark line was drawn");
    for &(column, row) in &lines {
        assert_eq!(buffer[(column, row)].fg, MARK_COLOR);
        assert!(
            (4..=buffer.area.height - 4).contains(&row),
            "a mark left the price plot at row {row}"
        );
    }

    let labelled: Vec<(u16, String)> = mark_rows(&buffer)
        .into_iter()
        .map(|row| (row, row_text(&buffer, row)))
        .collect();
    for label in ["moon 500.00", "target 118.00", "near 117.60"] {
        assert_eq!(
            labelled
                .iter()
                .filter(|(_, text)| text.contains(label))
                .count(),
            1,
            "{label} is drawn once on its own row: {labelled:?}"
        );
    }
    assert!(
        row_text(&buffer, 4).contains("120.00") && row_text(&buffer, 18).contains("95.00"),
        "the price axis survives beside the marks: {labelled:?}"
    );
}

#[tokio::test]
async fn marks_stay_inside_the_plot_at_every_panel_size() {
    let view = marked_view().await;
    for (columns, rows) in [(40, 14), (26, 11), (110, 24)] {
        let buffer = draw_marked(&view, columns, rows);
        let marked = mark_rows(&buffer);
        assert!(!marked.is_empty(), "{columns}x{rows} drew no mark");
        for row in marked {
            assert!(
                (4..=rows - 4).contains(&row),
                "{columns}x{rows} drew a mark outside the price plot at row {row}"
            );
        }
    }
}
