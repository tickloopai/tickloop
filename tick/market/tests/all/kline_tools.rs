//! The Kline Tick Tools over the in-memory duplex transport: a date-range
//! query the Agent recovers history with, and the two actions that annotate the
//! running Panel.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use ratatui::{Terminal, backend::TestBackend};
use serde_json::{Value, json};
use tick_market::{
    args::{KlineArgs, OnceArgs},
    commands::kline,
    config::Settings,
    metadata::TickKind,
};
use tick_protocol::INVALID_PARAMS;
use tick_provider::{
    api::{
        Candle, CandleAdjust, CandleHistoryRequest, CandlePeriod, CandleRequest, DateRange,
        MarketEvent, Quote, QuoteApi,
    },
    error::Result,
};
use tick_sdk::stream::TickStream;
use tokio::io::AsyncWriteExt;

const FIRST_DAY: (i32, time::Month, u8) = (2026, time::Month::September, 8);
const LAST_DAY: (i32, time::Month, u8) = (2026, time::Month::September, 10);

fn date(parts: (i32, time::Month, u8)) -> time::Date {
    time::Date::from_calendar_date(parts.0, parts.1, parts.2).unwrap()
}

/// One deterministic candle per calendar day of the requested range.
fn history(range: DateRange) -> Vec<Candle> {
    let mut candles = Vec::new();
    let mut day = range.start;
    while day <= range.end {
        let offset = i64::from(day.ordinal());
        candles.push(Candle {
            timestamp: day.midnight().assume_utc(),
            open: (100 + offset).into(),
            high: (110 + offset).into(),
            low: (95 + offset).into(),
            close: (105 + offset).into(),
            volume: 100 + offset,
            turnover: 1000.into(),
        });
        day += time::Duration::days(1);
    }
    candles
}

struct Candles {
    events: Mutex<Option<tokio::sync::mpsc::UnboundedReceiver<MarketEvent>>>,
    ranges: Arc<Mutex<Vec<CandleHistoryRequest>>>,
}

#[async_trait]
impl QuoteApi for Candles {
    async fn quotes(&self, _: &[String]) -> Result<Vec<Quote>> {
        unreachable!("the Kline Tick never asks for quotes")
    }
    async fn candles(&self, request: &CandleRequest) -> Result<Vec<Candle>> {
        Ok(history(DateRange {
            start: date(FIRST_DAY),
            end: date(LAST_DAY),
        })
        .into_iter()
        .take(request.count)
        .collect())
    }
    async fn candles_by_date(&self, request: &CandleHistoryRequest) -> Result<Vec<Candle>> {
        self.ranges.lock().unwrap().push(request.clone());
        Ok(history(request.range))
    }
    async fn subscribe_candles(&self, _: &CandleRequest) -> Result<bool> {
        Ok(false)
    }
    async fn take_events(&self) -> Result<tokio::sync::mpsc::UnboundedReceiver<MarketEvent>> {
        Ok(self.events.lock().unwrap().take().unwrap())
    }
}

struct Served {
    frames: Vec<Value>,
    ranges: Vec<CandleHistoryRequest>,
}

impl Served {
    fn answer(&self, id: u64) -> &Value {
        self.frames
            .iter()
            .find(|frame| frame["id"] == id)
            .unwrap_or_else(|| panic!("request {id} was answered: {:?}", self.frames))
    }

    fn updates(&self) -> Vec<&Value> {
        self.frames
            .iter()
            .filter(|frame| frame["method"] == "tick/update")
            .map(|frame| &frame["params"])
            .collect()
    }

    fn marks(&self) -> Value {
        self.updates().last().unwrap()["data"]["marks"].clone()
    }

    /// The seq of the last Tick Update. The latest-only stream may drop
    /// intermediate records, so the seq, not the delivered count, says how
    /// many updates the Panel published.
    fn published(&self) -> u64 {
        self.updates().last().unwrap()["seq"].as_u64().unwrap()
    }
}

/// Runs the Kline Tick over an in-memory duplex transport, feeds it the given
/// request lines, and returns every frame it wrote back.
async fn serve(requests: &[Value]) -> Served {
    let (mut client_requests, tick_requests) = tokio::io::duplex(8 * 1024);
    let (tick_responses, mut client_responses) = tokio::io::duplex(256 * 1024);
    let mut session = TickStream::duplex(TickKind::Kline, tick_requests, tick_responses);
    for request in requests {
        let mut line = serde_json::to_vec(request).unwrap();
        line.push(b'\n');
        client_requests.write_all(&line).await.unwrap();
    }
    client_requests.shutdown().await.unwrap();

    let (_push, market) = tokio::sync::mpsc::unbounded_channel();
    let ranges = Arc::new(Mutex::new(Vec::new()));
    let api = Candles {
        events: Mutex::new(Some(market)),
        ranges: Arc::clone(&ranges),
    };
    let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
    kline::run_live(
        &KlineArgs {
            symbol: "700.HK".into(),
            period: CandlePeriod::Day,
            count: Some(3),
            adjust: CandleAdjust::None,
            event: Default::default(),
            output: OnceArgs::default(),
        },
        &api,
        &Settings::default(),
        &mut terminal,
        futures_util::stream::pending(),
        &mut session,
    )
    .await
    .unwrap();
    session.close().await;

    let mut bytes = Vec::new();
    tokio::io::AsyncReadExt::read_to_end(&mut client_responses, &mut bytes)
        .await
        .unwrap();
    let frames = String::from_utf8(bytes)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    let ranges = ranges.lock().unwrap().clone();
    Served { frames, ranges }
}

fn call(id: u64, tool: &str, args: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "method": "tick/call", "params": {"tool": tool, "args": args, "revision": "1"}})
}

#[tokio::test]
async fn kline_recovers_updates_with_candles_and_declares_event_and_mark_tools() {
    let served = serve(&[json!({"jsonrpc": "2.0", "id": 1, "method": "tick/describe"})]).await;

    let describe = served.answer(1);
    assert_eq!(describe["result"]["kind"], "kline");
    assert_eq!(describe["result"]["recover_with"], "candles");
    let tools = describe["result"]["tools"].as_array().unwrap();
    let declared: Vec<(&str, bool)> = tools
        .iter()
        .map(|tool| {
            (
                tool["name"].as_str().unwrap(),
                tool["mutates"].as_bool().unwrap(),
            )
        })
        .collect();
    assert_eq!(
        declared,
        [
            ("candles", false),
            ("mark", true),
            ("unmark", true),
            ("event_history", false),
            ("set_event", true),
            ("set_context", true),
            ("set_view", true)
        ]
    );

    let candles = tools[0]["input_schema"].to_string();
    assert!(
        candles.contains("\"15m\"") && candles.contains("\"forward\""),
        "the tool speaks the Tick's period and adjust vocabulary: {candles}"
    );
    assert_eq!(
        tools[0]["input_schema"]["required"],
        json!(["symbol", "from", "to"])
    );
    assert_eq!(
        tools[1]["input_schema"]["required"],
        json!(["price", "label"])
    );
    assert_eq!(tools[2]["input_schema"]["required"], json!(["label"]));
}

#[tokio::test]
async fn the_candles_tool_answers_a_date_range_in_the_panel_shape() {
    let served = serve(&[call(
        4,
        "candles",
        json!({"symbol": "700", "period": "week", "adjust": "forward",
               "from": "2026-09-08", "to": "2026-09-10"}),
    )])
    .await;

    let answer = served.answer(4);
    assert_eq!(answer["result"]["symbol"], "700.HK");
    assert_eq!(answer["result"]["period"], "week");
    assert_eq!(answer["result"]["adjust"], "forward");
    let rows = answer["result"]["candles"].as_array().unwrap();
    assert_eq!(rows.len(), 3);
    assert_eq!(rows[0]["timestamp"], "2026-09-08T00:00:00Z");
    assert_eq!(rows[2]["close"], "358");

    assert_eq!(served.ranges.len(), 1);
    assert_eq!(served.ranges[0].symbol, "700.HK");
    assert_eq!(served.ranges[0].period, CandlePeriod::Week);
    assert_eq!(served.ranges[0].adjust, CandleAdjust::Forward);
    assert_eq!(served.ranges[0].range.start, date(FIRST_DAY));
    assert_eq!(served.ranges[0].range.end, date(LAST_DAY));
}

#[tokio::test]
async fn the_state_summarises_the_window_that_the_candles_tool_reads_in_full() {
    let state = serve(&[]).await.updates().last().unwrap()["data"].clone();
    assert!(state.get("candles").is_none(), "{state}");
    assert_eq!(
        state["history"],
        json!({"candles": 3, "first": "2026-09-08T00:00:00Z", "last": "2026-09-10T00:00:00Z", "tool": "candles"})
    );
    let day = |field: &str| state["history"][field].as_str().unwrap()[..10].to_owned();

    let served = serve(&[call(
        1,
        state["history"]["tool"].as_str().unwrap(),
        json!({"symbol": state["symbol"], "from": day("first"), "to": day("last")}),
    )])
    .await;
    let rows = served.answer(1)["result"]["candles"].as_array().unwrap();
    assert_eq!(rows.len(), 3);
    assert_eq!(rows.last(), Some(&state["latest"]));
    assert_eq!(rows[1]["close"], state["previous_close"]);
}

#[tokio::test]
async fn a_reversed_unbounded_or_unparsable_date_range_never_reaches_the_market() {
    let served = serve(&[
        call(
            1,
            "candles",
            json!({"symbol": "700.HK", "from": "2026-09-10", "to": "2026-09-08"}),
        ),
        call(
            2,
            "candles",
            json!({"symbol": "700.HK", "from": "2000-01-01", "to": "2026-09-08"}),
        ),
        call(
            3,
            "candles",
            json!({"symbol": "700.HK", "from": "yesterday", "to": "2026-09-08"}),
        ),
    ])
    .await;

    for (id, expected) in [(1, "from"), (2, "days"), (3, "YYYY-MM-DD")] {
        let message = served.answer(id)["error"]["message"]
            .as_str()
            .unwrap()
            .to_owned();
        assert!(message.contains(expected), "{id}: {message}");
    }
    assert!(
        served.ranges.is_empty(),
        "no market request was made: {:?}",
        served.ranges
    );
}

#[tokio::test]
async fn marking_a_price_changes_the_panel_and_publishes_one_tick_update() {
    let served = serve(&[
        call(1, "mark", json!({"price": 190.5, "label": "target"})),
        call(2, "mark", json!({"price": 88.0, "label": "stop"})),
        call(3, "unmark", json!({"label": "target"})),
        call(4, "unmark", json!({"label": "target"})),
    ])
    .await;

    assert_eq!(
        served.answer(1)["result"]["marks"],
        json!([{"label": "target", "price": 190.5}])
    );
    assert_eq!(
        served.answer(3)["result"]["marks"],
        json!([{"label": "stop", "price": 88.0}])
    );
    let missing = served.answer(4)["error"]["message"].as_str().unwrap();
    assert!(missing.contains("target"), "{missing}");

    assert_eq!(
        served.published(),
        4,
        "the snapshot plus one update per accepted action, none for the rejected one"
    );
    assert_eq!(served.marks(), json!([{"label": "stop", "price": 88.0}]));
}

#[tokio::test]
async fn re_marking_a_label_moves_the_existing_line_instead_of_adding_one() {
    let served = serve(&[
        call(1, "mark", json!({"price": 190.5, "label": "target"})),
        call(2, "mark", json!({"price": 205.0, "label": "target"})),
    ])
    .await;

    assert_eq!(served.marks(), json!([{"label": "target", "price": 205.0}]));
    assert_eq!(served.published(), 3, "each move published its own update");
}

#[tokio::test]
async fn a_new_kline_instance_starts_without_the_marks_of_the_previous_one() {
    let marked = serve(&[call(1, "mark", json!({"price": 190.5, "label": "target"}))]).await;
    assert_eq!(marked.marks(), json!([{"label": "target", "price": 190.5}]));

    let restarted = serve(&[]).await;
    assert_eq!(restarted.marks(), json!([]));
}

#[tokio::test]
async fn an_action_the_schema_rejects_never_reaches_the_panel() {
    let served = serve(&[call(
        1,
        "mark",
        json!({"price": "expensive", "label": "target"}),
    )])
    .await;

    let invalid = served.answer(1);
    assert_eq!(invalid["error"]["code"], INVALID_PARAMS);
    assert_eq!(
        invalid["error"]["data"]["schema"]["properties"]["price"]["type"], "number",
        "the declared schema travels with the error: {invalid:?}"
    );
    assert_eq!(
        served.published(),
        1,
        "the rejected action published nothing after the snapshot"
    );
}
