use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, AtomicUsize, Ordering},
    },
    time::Duration,
};

use async_trait::async_trait;
use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
use ratatui::{Terminal, backend::TestBackend};
use serde_json::{Value, json};
use tick_command::{
    cli::TickCommand,
    connecting::{DataSource, connect_and_run},
    entry::Services,
};
use tick_market::{
    args::{OnceArgs, QuoteArgs},
    config::Settings,
    metadata::TickKind,
};
use tick_provider::{
    api::{MarketEvent, Quote, QuoteApi},
    error::{Error, Result},
    exit_code,
    preferences::Language,
};
use tick_sdk::stream::{Records, TickStream};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt},
    sync::mpsc,
};
use tokio_stream::wrappers::UnboundedReceiverStream;

/// What one attempt to connect comes to.
enum Attempt {
    Fails(Error),
    /// Connects to quotes whose first fetch fails with `first_fetch` when there is one.
    Connects {
        first_fetch: Option<Error>,
    },
}

struct Quotes {
    first_fetch: Mutex<Option<Error>>,
    events: Mutex<Option<mpsc::UnboundedReceiver<MarketEvent>>>,
}

#[async_trait]
impl QuoteApi for Quotes {
    async fn quotes(&self, _: &[String]) -> Result<Vec<Quote>> {
        if let Some(error) = self.first_fetch.lock().unwrap().take() {
            return Err(error);
        }
        Ok(serde_json::from_str(
            r#"[{"symbol":"TSLA.US","name":"Tesla","latest_price":"100","previous_close":"90"}]"#,
        )
        .unwrap())
    }
    async fn subscribe_quotes(&self, _: &[String]) -> Result<()> {
        Ok(())
    }
    async fn take_events(&self) -> Result<mpsc::UnboundedReceiver<MarketEvent>> {
        Ok(self.events.lock().unwrap().take().unwrap())
    }
}

type Pushes = Arc<Mutex<Option<mpsc::UnboundedSender<MarketEvent>>>>;

/// A data source whose attempts the test scripts one by one; an attempt waits until the test
/// says how it ends.
struct Source {
    attempts: mpsc::UnboundedReceiver<Attempt>,
    connects: Arc<AtomicUsize>,
    login: Arc<AtomicU64>,
    /// Where the test pushes market events to the latest connection.
    pushes: Pushes,
}

impl DataSource for Source {
    type Connection = Quotes;

    fn name(&self) -> &str {
        "Longbridge"
    }

    fn login_hint(&self) -> &str {
        "log in from /ticks"
    }

    fn stored_login(&self) -> Option<u64> {
        Some(self.login.load(Ordering::SeqCst))
    }

    async fn connect(&mut self) -> Result<Quotes> {
        self.connects.fetch_add(1, Ordering::SeqCst);
        match self
            .attempts
            .recv()
            .await
            .expect("every attempt is scripted")
        {
            Attempt::Fails(error) => Err(error),
            Attempt::Connects { first_fetch } => {
                let (pushes, events) = mpsc::unbounded_channel();
                *self.pushes.lock().unwrap() = Some(pushes);
                Ok(Quotes {
                    first_fetch: Mutex::new(first_fetch),
                    events: Mutex::new(Some(events)),
                })
            }
        }
    }

    fn services(quotes: &Quotes) -> Services<'_> {
        Services {
            quote: Some(quotes),
            trade: None,
            news: None,
            options: None,
            community: None,
        }
    }
}

struct Script {
    attempts: mpsc::UnboundedSender<Attempt>,
    connects: Arc<AtomicUsize>,
    login: Arc<AtomicU64>,
    pushes: Pushes,
    keys: mpsc::UnboundedSender<std::io::Result<Event>>,
}

impl Script {
    fn attempt(&self, attempt: Attempt) {
        self.attempts.send(attempt).unwrap();
    }

    fn push(&self, event: MarketEvent) {
        let pushes = self.pushes.lock().unwrap();
        pushes.as_ref().expect("connected").send(event).unwrap();
    }

    fn quit(&self) {
        self.keys
            .send(Ok(Event::Key(KeyEvent::new(
                KeyCode::Char('q'),
                KeyModifiers::NONE,
            ))))
            .unwrap();
    }
}

fn source() -> (
    Script,
    Source,
    UnboundedReceiverStream<std::io::Result<Event>>,
) {
    let (attempts, scripted) = mpsc::unbounded_channel();
    let (keys, input) = mpsc::unbounded_channel();
    let connects = Arc::new(AtomicUsize::new(0));
    let login = Arc::new(AtomicU64::new(1));
    let pushes = Pushes::default();
    (
        Script {
            attempts,
            connects: Arc::clone(&connects),
            login: Arc::clone(&login),
            pushes: Arc::clone(&pushes),
            keys,
        },
        Source {
            attempts: scripted,
            connects,
            login,
            pushes,
        },
        UnboundedReceiverStream::new(input),
    )
}

fn english() -> Settings {
    Settings {
        language: Language::En,
        ..Settings::default()
    }
}

fn quote_tick() -> TickCommand {
    TickCommand::Quote(QuoteArgs {
        symbols: vec!["TSLA.US".into()],
        output: OnceArgs::default(),
    })
}

/// Waits, in the paused test clock, until `records` holds `count` Tick Updates.
async fn published(records: &Records, count: usize) -> Vec<Value> {
    for _ in 0..600 {
        if records.len() >= count {
            return records.all();
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("expected {count} Tick Updates: {:?}", records.all());
}

fn screen(terminal: &Terminal<TestBackend>) -> String {
    terminal
        .backend()
        .buffer()
        .content
        .iter()
        .map(|cell| cell.symbol())
        .collect()
}

/// Until the first content, each failed attempt to reach the source is retried with a growing
/// delay; the Agent hears of the failure once, again only when its reason changes, and then of
/// the content. A first fetch that fails counts as an attempt that failed.
#[tokio::test(start_paused = true)]
async fn an_unreachable_source_is_retried_and_each_new_reason_is_published_once() {
    let (script, source, input) = source();
    let mut updates = TickStream::in_memory(TickKind::Quote);
    let records = updates.records();
    let mut terminal = Terminal::new(TestBackend::new(80, 12)).unwrap();
    let started = tokio::time::Instant::now();
    let settings = english();
    let run = connect_and_run(
        quote_tick(),
        &settings,
        &mut terminal,
        input,
        &mut updates,
        source,
    );
    let host = async {
        script.attempt(Attempt::Fails(Error::network("connection refused")));
        let failed = published(&records, 1).await;
        assert_eq!(
            failed[0]["data"],
            json!({"connection": {"state": "retrying", "error": "connection refused"}})
        );
        script.attempt(Attempt::Fails(Error::network("connection refused")));
        script.attempt(Attempt::Connects {
            first_fetch: Some(Error::network("connect timeout")),
        });
        script.attempt(Attempt::Connects { first_fetch: None });
        let shown = published(&records, 3).await;
        script.quit();
        shown
    };
    let (result, shown) = tokio::join!(run, host);
    result.unwrap();
    assert_eq!(
        shown[1]["data"],
        json!({"connection": {"state": "retrying", "error": "connect timeout"}}),
        "the same reason again publishes nothing: {shown:?}"
    );
    assert_eq!(shown[1]["seq"], 2);
    assert_eq!(shown[2]["seq"], 3);
    assert_eq!(shown[2]["data"][0]["symbol"], "TSLA.US", "{shown:?}");
    assert_eq!(records.len(), 3);
    assert_eq!(script.connects.load(Ordering::SeqCst), 4);
    assert!(
        started.elapsed() >= Duration::from_secs(1 + 2 + 4),
        "it backs off between attempts: {:?}",
        started.elapsed()
    );
    assert!(screen(&terminal).contains("Tesla"), "{}", screen(&terminal));
}

/// While it retries, the view says which attempt is under way and why the last one failed.
#[tokio::test(start_paused = true)]
async fn the_view_shows_the_attempt_and_the_last_error_and_an_unexpected_failure_ends_the_tick() {
    let (script, source, input) = source();
    let mut updates = TickStream::in_memory(TickKind::Quote);
    let mut terminal = Terminal::new(TestBackend::new(80, 12)).unwrap();
    script.attempt(Attempt::Fails(Error::network("connection refused")));
    script.attempt(Attempt::Fails(Error::usage("invalid configuration")));
    let error = connect_and_run(
        quote_tick(),
        &english(),
        &mut terminal,
        input,
        &mut updates,
        source,
    )
    .await
    .unwrap_err();
    assert_eq!(
        (error.code, error.message.as_str()),
        (exit_code::USAGE, "invalid configuration")
    );
    let screen = screen(&terminal);
    assert!(
        screen.contains("Connecting to Longbridge (attempt 2)"),
        "{screen}"
    );
    assert!(
        screen.contains("Last error: connection refused"),
        "{screen}"
    );
}

/// A refused login is published once with how to log in again; the Tick then only rereads the
/// stored login, and connects again once the user logged in.
#[tokio::test(start_paused = true)]
async fn a_refused_login_waits_for_a_new_login_and_then_connects() {
    let (script, source, input) = source();
    let mut updates = TickStream::in_memory(TickKind::Quote);
    let records = updates.records();
    let mut terminal = Terminal::new(TestBackend::new(80, 12)).unwrap();
    let settings = english();
    let run = connect_and_run(
        quote_tick(),
        &settings,
        &mut terminal,
        input,
        &mut updates,
        source,
    );
    let host = async {
        script.attempt(Attempt::Fails(Error::auth(
            "authorization expired or revoked",
        )));
        let refused = published(&records, 1).await;
        tokio::time::sleep(Duration::from_secs(35)).await;
        let connects_while_refused = script.connects.load(Ordering::SeqCst);
        script.login.store(2, Ordering::SeqCst);
        script.attempt(Attempt::Connects { first_fetch: None });
        let shown = published(&records, 2).await;
        script.quit();
        (refused, connects_while_refused, shown)
    };
    let (result, (refused, connects_while_refused, shown)) = tokio::join!(run, host);
    result.unwrap();
    assert_eq!(
        refused[0]["data"],
        json!({"connection": {"state": "not_authorized", "hint": "log in from /ticks"}})
    );
    assert_eq!(
        connects_while_refused, 1,
        "the same stored login is not tried again"
    );
    assert_eq!(shown[1]["data"][0]["symbol"], "TSLA.US", "{shown:?}");
    assert_eq!(records.len(), 2);
}

/// A login refused after the Tick showed content does not end it either: the Agent hears that
/// the login was refused, and the content returns once the user logged in again.
#[tokio::test(start_paused = true)]
async fn a_login_refused_after_content_waits_for_a_new_login() {
    let (script, source, input) = source();
    let mut updates = TickStream::in_memory(TickKind::Quote);
    let records = updates.records();
    let mut terminal = Terminal::new(TestBackend::new(80, 12)).unwrap();
    let settings = english();
    let run = connect_and_run(
        quote_tick(),
        &settings,
        &mut terminal,
        input,
        &mut updates,
        source,
    );
    let host = async {
        script.attempt(Attempt::Connects { first_fetch: None });
        published(&records, 1).await;
        script.push(MarketEvent::Failure {
            code: exit_code::NOT_AUTHORIZED,
            message: "authorization expired or revoked".into(),
        });
        let refused = published(&records, 2).await;
        script.login.store(2, Ordering::SeqCst);
        script.attempt(Attempt::Connects { first_fetch: None });
        let shown = published(&records, 3).await;
        script.quit();
        (refused, shown)
    };
    let (result, (refused, shown)) = tokio::join!(run, host);
    result.unwrap();
    assert_eq!(
        refused[1]["data"],
        json!({"connection": {"state": "not_authorized", "hint": "log in from /ticks"}})
    );
    assert_eq!(shown[2]["data"][0]["symbol"], "TSLA.US", "{shown:?}");
    assert_eq!(script.connects.load(Ordering::SeqCst), 2);
}

/// A request the source turns down before any content, such as an unknown symbol, is not asked
/// again: the Agent hears the reason once, the view shows it, and the Tick stays open.
#[tokio::test(start_paused = true)]
async fn a_request_the_source_turns_down_is_shown_and_not_asked_again() {
    let (script, source, input) = source();
    let mut updates = TickStream::in_memory(TickKind::Quote);
    let records = updates.records();
    let mut terminal = Terminal::new(TestBackend::new(80, 12)).unwrap();
    let settings = english();
    let run = connect_and_run(
        quote_tick(),
        &settings,
        &mut terminal,
        input,
        &mut updates,
        source,
    );
    let host = async {
        script.attempt(Attempt::Connects {
            first_fetch: Some(Error::rejected("invalid symbol")),
        });
        published(&records, 1).await;
        tokio::time::sleep(Duration::from_secs(120)).await;
        let connects = script.connects.load(Ordering::SeqCst);
        script.quit();
        connects
    };
    let (result, connects) = tokio::join!(run, host);
    result.unwrap();
    assert_eq!(connects, 1, "the turned-down request is not asked again");
    let records = records.all();
    assert_eq!(records.len(), 1, "{records:?}");
    assert_eq!(
        records[0]["data"],
        json!({"connection": {"state": "failed", "error": "invalid symbol"}})
    );
    let screen = screen(&terminal);
    assert!(
        screen.contains("Longbridge turned this Tick's request down"),
        "{screen}"
    );
    assert!(screen.contains("invalid symbol"), "{screen}");
}

/// The view of a refused login says to log in again and how.
#[tokio::test(start_paused = true)]
async fn the_view_of_a_refused_login_says_how_to_log_in() {
    let (script, source, input) = source();
    let mut updates = TickStream::in_memory(TickKind::Quote);
    let mut terminal = Terminal::new(TestBackend::new(80, 12)).unwrap();
    script.attempt(Attempt::Fails(Error::auth("authorization expired")));
    let settings = english();
    let run = connect_and_run(
        quote_tick(),
        &settings,
        &mut terminal,
        input,
        &mut updates,
        source,
    );
    let host = async {
        tokio::time::sleep(Duration::from_secs(1)).await;
        script.quit();
    };
    let (result, ()) = tokio::join!(run, host);
    result.unwrap();
    let screen = screen(&terminal);
    assert!(screen.contains("Log in to Longbridge again"), "{screen}");
    assert!(screen.contains("log in from /ticks"), "{screen}");
}

/// Before the source is reached the host still gets its answers: the description and no data,
/// and a Tick Tool call fails at once with why; closing the stream ends the Tick.
#[tokio::test]
async fn the_host_is_answered_while_the_source_is_not_reached() {
    let (_script, source, input) = source();
    let (mut host_requests, tick_requests) = tokio::io::duplex(8 * 1024);
    let (tick_responses, host_responses) = tokio::io::duplex(64 * 1024);
    let mut answers = tokio::io::BufReader::new(host_responses).lines();
    let mut updates = TickStream::duplex(TickKind::Quote, tick_requests, tick_responses);
    let mut terminal = Terminal::new(TestBackend::new(80, 12)).unwrap();
    let settings = english();
    let run = connect_and_run(
        quote_tick(),
        &settings,
        &mut terminal,
        input,
        &mut updates,
        source,
    );
    let host = async {
        let mut answer = async |request: Value| {
            host_requests
                .write_all(format!("{request}\n").as_bytes())
                .await
                .unwrap();
            let line = tokio::time::timeout(Duration::from_secs(1), answers.next_line())
                .await
                .expect("answered within a second")
                .unwrap()
                .unwrap();
            serde_json::from_str::<Value>(&line).unwrap()
        };
        let described = answer(json!({"jsonrpc": "2.0", "id": 1, "method": "tick/describe"})).await;
        let read = answer(json!({"jsonrpc": "2.0", "id": 2, "method": "tick/read"})).await;
        let tool = described["result"]["tools"][0]["name"].clone();
        let called = answer(json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "tick/call",
            "params": {"tool": tool, "args": {}, "revision": described["result"]["revision"]},
        }))
        .await;
        drop(host_requests);
        (read, called)
    };
    let (result, (read, called)) = tokio::join!(run, host);
    result.unwrap();
    assert_eq!(read["result"], json!({"seq": 0, "data": null}));
    assert_eq!(called["id"], 3);
    assert_eq!(called["error"]["code"], tick_protocol::TOOL_FAILED);
    assert_eq!(
        called["error"]["message"],
        "Longbridge is not connected yet"
    );
    assert_eq!(called["error"]["data"]["code"], exit_code::NETWORK);
}
