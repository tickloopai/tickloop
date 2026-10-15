use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
use tick_market::{
    args::{KlineArgs, OnceArgs},
    commands::kline,
    config::Settings,
    metadata::TickKind,
};
use tick_provider::api::{CandleAdjust, CandlePeriod};
use tick_sdk::stream::TickStream;
use tick_test_support::MockApi;

#[tokio::test(start_paused = true)]
async fn kline_keeps_streaming_after_switching_candle_periods() {
    let api = MockApi::new().unwrap();
    let (keys, input) = tokio::sync::mpsc::unbounded_channel();
    let producer = tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        keys.send(Ok(Event::Key(KeyEvent::new(
            KeyCode::Char('5'),
            KeyModifiers::NONE,
        ))))
        .unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(2200)).await;
        keys.send(Ok(Event::Key(KeyEvent::new(
            KeyCode::Char('q'),
            KeyModifiers::NONE,
        ))))
        .unwrap();
    });
    let args = KlineArgs {
        symbol: "700.HK".into(),
        period: CandlePeriod::OneMinute,
        count: Some(30),
        adjust: CandleAdjust::None,
        event: Default::default(),
        output: OnceArgs::default(),
    };
    let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(45, 14)).unwrap();
    let mut events = TickStream::in_memory(TickKind::Kline);
    kline::run_live(
        &args,
        &api,
        &Settings::default(),
        &mut terminal,
        tokio_stream::wrappers::UnboundedReceiverStream::new(input),
        &mut events,
    )
    .await
    .unwrap();
    producer.await.unwrap();
    let records = events.records().all();
    let switched: Vec<_> = records
        .iter()
        .filter(|record| record["data"]["period"] == "5m")
        .collect();
    assert!(
        switched.len() >= 3,
        "switch snapshot and continuing updates: {records:?}"
    );
    assert_ne!(
        switched[0]["data"]["latest"],
        switched.last().unwrap()["data"]["latest"]
    );
    let screen: String = terminal
        .backend()
        .buffer()
        .content
        .iter()
        .map(|cell| cell.symbol())
        .collect();
    assert!(screen.contains("700.HK · 5m"), "{screen}");
}
