use std::{
    pin::Pin,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    task::{Context, Poll, Waker},
};

use async_trait::async_trait;
use ratatui::{Terminal, backend::TestBackend};
use serde_json::{Value, json};
use tick_market::{
    args::{OnceArgs, QuoteArgs},
    commands::quote,
    config::Settings,
    metadata::TickKind,
};
use tick_protocol::{
    CALL_METHOD, INVALID_PARAMS, METHOD_NOT_FOUND, NATIVE_PROTOCOL_VERSION, REVISION_MISMATCH,
};
use tick_provider::{
    api::{MarketEvent, Quote, QuoteApi},
    error::Result,
};
use tick_sdk::stream::TickStream;
use tokio::io::{AsyncWrite, AsyncWriteExt};

struct Quotes(Mutex<Option<tokio::sync::mpsc::UnboundedReceiver<MarketEvent>>>);

#[async_trait]
impl QuoteApi for Quotes {
    async fn quotes(&self, _: &[String]) -> Result<Vec<Quote>> {
        Ok(serde_json::from_value(
            json!([{"symbol": "AAPL.US", "name": "Apple", "latest_price": "100", "previous_close": "90"}]),
        )
        .unwrap())
    }
    async fn subscribe_quotes(&self, _: &[String]) -> Result<()> {
        Ok(())
    }
    async fn take_events(&self) -> Result<tokio::sync::mpsc::UnboundedReceiver<MarketEvent>> {
        Ok(self.0.lock().unwrap().take().unwrap())
    }
}

/// Runs the Quote Tick over an in-memory duplex transport, feeds it the given
/// request lines, and returns every frame the Tick wrote back.
async fn serve(requests: &[Value]) -> Vec<Value> {
    let (mut client_requests, tick_requests) = tokio::io::duplex(8 * 1024);
    let (tick_responses, mut client_responses) = tokio::io::duplex(64 * 1024);
    let mut session = TickStream::duplex(TickKind::Quote, tick_requests, tick_responses);
    for request in requests {
        let mut line = serde_json::to_vec(request).unwrap();
        line.push(b'\n');
        client_requests.write_all(&line).await.unwrap();
    }
    client_requests.write_all(b"not-json\n").await.unwrap();
    client_requests.shutdown().await.unwrap();

    let (_push, market) = tokio::sync::mpsc::unbounded_channel();
    let api = Quotes(Mutex::new(Some(market)));
    let mut terminal = Terminal::new(TestBackend::new(60, 12)).unwrap();
    quote::run_live(
        &QuoteArgs {
            symbols: vec!["AAPL.US".into()],
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
    String::from_utf8(bytes)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

fn responses(frames: &[Value]) -> Vec<&Value> {
    frames
        .iter()
        .filter(|frame| frame.get("id").is_some())
        .collect()
}

/// A `tick/call` against the shipped catalog revision.
fn call(id: u64, params: Value) -> Value {
    let mut params = params;
    params["revision"] = json!("1");
    json!({"jsonrpc": "2.0", "id": id, "method": CALL_METHOD, "params": params})
}

fn updates(frames: &[Value]) -> Vec<&Value> {
    frames
        .iter()
        .filter(|frame| frame["method"] == "tick/update")
        .map(|frame| &frame["params"])
        .collect()
}

#[tokio::test]
async fn describe_answers_with_protocol_three_and_the_quote_tool_catalog() {
    let frames = serve(&[json!({"jsonrpc": "2.0", "id": 1, "method": "tick/describe"})]).await;

    let describe = responses(&frames)
        .into_iter()
        .find(|frame| frame["id"] == 1)
        .expect("describe was answered");
    assert_eq!(describe["jsonrpc"], "2.0");
    assert_eq!(describe["result"]["kind"], "quote");
    assert_eq!(describe["result"]["protocol"], NATIVE_PROTOCOL_VERSION);
    assert_eq!(describe["result"]["revision"], "1");
    assert_eq!(describe["result"]["recover_with"], "quotes");
    let tools = describe["result"]["tools"].as_array().unwrap();
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0]["name"], "quotes");
    assert_eq!(tools[0]["mutates"], false);
    assert!(tools[0]["description"].as_str().unwrap().contains("price"));
    assert_eq!(
        tools[0]["input_schema"]["properties"]["symbols"]["type"],
        "array"
    );
    assert_eq!(tools[0]["input_schema"]["required"], json!(["symbols"]));
}

#[tokio::test]
async fn a_query_tool_call_answers_with_the_quote_table_json() {
    let frames = serve(&[call(
        7,
        json!({"tool": "quotes", "args": {"symbols": ["AAPL"]}}),
    )])
    .await;

    let answer = responses(&frames)
        .into_iter()
        .find(|frame| frame["id"] == 7)
        .expect("the quotes call was answered");
    let rows = answer["result"].as_array().expect("a Quote table");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["symbol"], "AAPL.US");
    assert_eq!(rows[0]["latest_price"], "100");
    assert_eq!(
        answer["result"],
        updates(&frames)[0]["data"],
        "the tool returns the same JSON the Quote Panel publishes"
    );
}

#[tokio::test]
async fn invalid_tool_arguments_are_rejected_with_the_declared_schema() {
    let frames = serve(&[
        call(1, json!({"tool": "quotes", "args": {}})),
        call(2, json!({"tool": "candles", "args": {}})),
    ])
    .await;

    let invalid = responses(&frames)
        .into_iter()
        .find(|frame| frame["id"] == 1)
        .expect("the invalid call was answered");
    assert_eq!(invalid["error"]["code"], INVALID_PARAMS);
    assert!(
        invalid["error"]["message"]
            .as_str()
            .unwrap()
            .contains("symbols"),
        "{invalid:?}"
    );
    assert_eq!(
        invalid["error"]["data"]["schema"]["properties"]["symbols"]["type"], "array",
        "the declared schema travels with the error: {invalid:?}"
    );

    let unknown = responses(&frames)
        .into_iter()
        .find(|frame| frame["id"] == 2)
        .expect("the unknown call was answered");
    assert_eq!(unknown["error"]["code"], INVALID_PARAMS);
}

#[tokio::test]
async fn a_call_naming_another_catalog_revision_is_refused_before_it_runs() {
    let frames = serve(&[json!({
        "jsonrpc": "2.0",
        "id": 7,
        "method": CALL_METHOD,
        "params": {"tool": "quotes", "args": {"symbols": ["AAPL"]}, "revision": "0"}
    })])
    .await;

    let refused = responses(&frames)
        .into_iter()
        .find(|frame| frame["id"] == 7)
        .expect("the stale call was answered");
    assert_eq!(refused["error"]["code"], REVISION_MISMATCH);
}

#[tokio::test]
async fn the_first_tick_update_carries_only_seq_data_and_at() {
    let frames = serve(&[]).await;

    let first = updates(&frames).first().copied().expect("a Tick Update");
    let mut fields: Vec<_> = first.as_object().unwrap().keys().collect();
    fields.sort();
    assert_eq!(fields, ["at", "data", "seq"]);
    assert_eq!(first["seq"], 1);
    assert_eq!(first["data"][0]["symbol"], "AAPL.US");
}

#[tokio::test]
async fn protocol_errors_are_answered_without_closing_the_stream() {
    let frames = serve(&[
        json!({"jsonrpc": "2.0", "id": 1, "method": "tick/nope"}),
        call(2, json!({"tool": "nope"})),
        json!({"jsonrpc": "2.0", "id": 3, "method": CALL_METHOD, "params": {}}),
        json!({"jsonrpc": "2.0", "id": 4, "method": "tick/describe"}),
    ])
    .await;

    let codes: Vec<(Value, i64)> = responses(&frames)
        .into_iter()
        .filter_map(|frame| {
            frame
                .get("error")
                .map(|error| (frame["id"].clone(), error["code"].as_i64().unwrap()))
        })
        .collect();
    assert_eq!(
        codes,
        [
            (json!(1), METHOD_NOT_FOUND),
            (json!(2), INVALID_PARAMS),
            (json!(3), INVALID_PARAMS),
        ],
        "the closing line that is not JSON goes unanswered"
    );
    assert!(
        responses(&frames)
            .into_iter()
            .any(|frame| frame["id"] == 4 && frame["result"]["protocol"] == 3),
        "the describe after the bad requests is still answered: {frames:?}"
    );
    assert_eq!(updates(&frames).len(), 1);
}

#[tokio::test]
async fn a_slow_consumer_receives_only_the_latest_update_and_the_seq_shows_the_gap() {
    let (writer, mut attempts) = BlockedWriter::new();
    let written = writer.written();
    let mut session = TickStream::duplex(TickKind::Quote, tokio::io::empty(), writer.clone());

    session.publish(json!({"step": 1})).unwrap();
    attempts
        .recv()
        .await
        .expect("the writer started the first frame");
    for step in 2..=4 {
        session.publish(json!({"step": step})).unwrap();
    }
    writer.release();
    session.close().await;

    let frames: Vec<Value> = String::from_utf8(written.lock().unwrap().clone())
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    let delivered: Vec<(u64, Value)> = updates(&frames)
        .into_iter()
        .map(|record| (record["seq"].as_u64().unwrap(), record["data"].clone()))
        .collect();
    assert_eq!(
        delivered,
        [(1, json!({"step": 1})), (4, json!({"step": 4}))],
        "two intermediate updates were dropped and the gap is visible"
    );
}

/// A consumer that parks on its first write until the test releases it, so the
/// latest-update slot is observably replaced while one frame is in flight.
#[derive(Clone)]
struct BlockedWriter {
    written: Arc<Mutex<Vec<u8>>>,
    attempts: tokio::sync::mpsc::UnboundedSender<()>,
    released: Arc<AtomicBool>,
    waker: Arc<Mutex<Option<Waker>>>,
}

impl BlockedWriter {
    fn new() -> (Self, tokio::sync::mpsc::UnboundedReceiver<()>) {
        let (attempts, receiver) = tokio::sync::mpsc::unbounded_channel();
        (
            Self {
                written: Arc::new(Mutex::new(Vec::new())),
                attempts,
                released: Arc::new(AtomicBool::new(false)),
                waker: Arc::new(Mutex::new(None)),
            },
            receiver,
        )
    }

    fn written(&self) -> Arc<Mutex<Vec<u8>>> {
        Arc::clone(&self.written)
    }

    fn release(&self) {
        self.released.store(true, Ordering::Release);
        if let Some(waker) = self.waker.lock().unwrap().take() {
            waker.wake();
        }
    }
}

impl AsyncWrite for BlockedWriter {
    fn poll_write(
        self: Pin<&mut Self>,
        context: &mut Context<'_>,
        buffer: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        if !self.released.load(Ordering::Acquire) {
            *self.waker.lock().unwrap() = Some(context.waker().clone());
            let _ = self.attempts.send(());
            return Poll::Pending;
        }
        self.written.lock().unwrap().extend_from_slice(buffer);
        Poll::Ready(Ok(buffer.len()))
    }

    fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}
