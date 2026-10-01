use std::sync::Arc;

use crossterm::event::KeyCode;
use serde_json::json;
use tickloop_ticks::{
    commands::options::{OptionsView, demo_api},
    config::Settings,
    futu,
    options::{
        self, Contract, MarketQuote, OptionLeg, OptionSide, OptionsApi, OptionsRequest,
        SnapshotStore, StrikePair,
    },
    research::{ResearchContext, ResearchStatus},
    runtime::{LiveView, NoMarketApi, ViewState},
    tools::{ToolCall, ToolDispatch},
};

fn research(period: &str, date: &str) -> ResearchContext {
    ResearchContext {
        symbol: "DEMO.US".into(),
        period_id: period.into(),
        release_date: Some(date.into()),
        release_session: tickloop_ticks::event_chart::EventSession::AfterHours,
        published_at: Some(format!("{date}T20:05:00Z")),
        status: ResearchStatus::Released,
        ..Default::default()
    }
}

fn event_snapshot(
    mut snapshot: options::OptionsSnapshot,
    context: &ResearchContext,
    at: &str,
    spot: f64,
) -> options::OptionsSnapshot {
    snapshot.research_context = Some(context.clone());
    snapshot.observed_at = at.into();
    snapshot.quote_time_zone = "UTC".into();
    snapshot.expiry = "2026-10-02".into();
    snapshot.underlying.last = Some(spot);
    snapshot.underlying.quote_time = Some(at.into());
    for row in &mut snapshot.rows {
        for leg in [&mut row.call, &mut row.put].into_iter().flatten() {
            leg.contract.expiry = snapshot.expiry.clone();
            if let Some(quote) = &mut leg.quote {
                quote.quote_time = Some(at.into());
            }
        }
    }
    snapshot.atm = options::straddle(&snapshot.underlying, &snapshot.rows, &snapshot.expiry);
    snapshot
}

#[tokio::test]
async fn event_history_filters_two_periods_by_original_quote_times_not_save_order() {
    let api = demo_api();
    let template = options::load(
        api.as_ref(),
        &OptionsRequest::new("DEMO.US", None, 21).unwrap(),
    )
    .await
    .unwrap();
    let temporary = tempfile::tempdir().unwrap();
    let store = SnapshotStore::new(Some(temporary.path().into())).unwrap();
    let old = research("FY2026-Q2", "2026-08-06");
    let new = research("FY2026-Q3", "2026-09-24");
    let before = event_snapshot(template.clone(), &old, "2026-08-06T19:55:00Z", 100.0);
    let after = event_snapshot(template.clone(), &old, "2026-08-06T20:10:00Z", 105.0);
    store.save(&after).unwrap();
    store.save(&before).unwrap();
    store
        .save(&event_snapshot(
            template.clone(),
            &new,
            "2026-09-24T19:55:00Z",
            95.0,
        ))
        .unwrap();
    let mut mixed = event_snapshot(template.clone(), &old, "2026-08-06T20:20:00Z", 150.0);
    mixed.underlying.quote_time = Some("2026-08-06T19:55:00Z".into());
    store.save(&mixed).unwrap();
    let report = store.event_history(&old).unwrap();
    assert_eq!(report.before.unwrap().underlying.last, Some(100.0));
    assert_eq!(report.after.unwrap().underlying.last, Some(105.0));
    assert!((report.saved_spot_change_percent.unwrap() - 5.0).abs() < 0.0001);
    assert_eq!(report.unclassified_count, 1);
    let report = store.event_history(&new).unwrap();
    assert_eq!(report.before.unwrap().underlying.last, Some(95.0));
    assert!(report.after.is_none());
    assert!(report.saved_spot_change_percent.is_none());
}

#[tokio::test]
async fn switching_historical_context_never_shows_current_chain_as_before_event() {
    let api = demo_api();
    let request = OptionsRequest::new("DEMO.US", None, 21).unwrap();
    let snapshot = options::load(api.as_ref(), &request).await.unwrap();
    let temporary = tempfile::tempdir().unwrap();
    let store = SnapshotStore::new(Some(temporary.path().into())).unwrap();
    let old = research("FY2026-Q2", "2026-08-06");
    let new = research("FY2026-Q3", "2026-09-24");
    store
        .save(&event_snapshot(
            snapshot.clone(),
            &old,
            "2026-08-06T19:55:00Z",
            100.0,
        ))
        .unwrap();
    let mut view = OptionsView::new(request, snapshot, api, store);
    view.act(ToolCall {
        tool: "set_context".into(),
        args: json!({"context":old}),
    })
    .await
    .unwrap();
    assert!(!view.table().rows.is_empty());
    view.act(ToolCall {
        tool: "set_context".into(),
        args: json!({"context":new}),
    })
    .await
    .unwrap();
    assert!(view.table().rows.is_empty());
    assert!(view.content()["event_history"]["before"].is_null());
    assert!(view.content()["current_snapshot"].is_object());
    let before = view.content();
    let mut state = ViewState::default();
    view.on_key(KeyCode::Char('c'), &NoMarketApi, &mut state)
        .await
        .unwrap();
    assert!(!view.table().rows.is_empty());
    view.on_key(KeyCode::Char('b'), &NoMarketApi, &mut state)
        .await
        .unwrap();
    assert!(view.table().rows.is_empty());
    assert_eq!(view.content(), before);
    view.on_key(KeyCode::Char('a'), &NoMarketApi, &mut state)
        .await
        .unwrap();
    let navigation = view.take_navigation().unwrap();
    assert_eq!(navigation.context.period_id, "FY2026-Q3");
    assert!(
        navigation
            .context
            .evidence
            .unwrap()
            .text
            .contains("\"missing\":true")
    );
    let mut different = new.clone();
    different.symbol = "OTHER.US".into();
    view.set_research_context(different).unwrap();
    assert!(view.content()["current_snapshot"].is_null());
    assert!(view.table().rows.is_empty());
}

#[tokio::test]
async fn unknown_release_time_does_not_guess_same_day_before_or_after() {
    let api = demo_api();
    let template = options::load(
        api.as_ref(),
        &OptionsRequest::new("DEMO.US", None, 21).unwrap(),
    )
    .await
    .unwrap();
    let temporary = tempfile::tempdir().unwrap();
    let store = SnapshotStore::new(Some(temporary.path().into())).unwrap();
    let mut context = research("FY2026-Q3", "2026-09-24");
    context.published_at = None;
    store
        .save(&event_snapshot(
            template.clone(),
            &context,
            "2026-09-24T19:55:00Z",
            100.0,
        ))
        .unwrap();
    store
        .save(&event_snapshot(
            template.clone(),
            &context,
            "2026-09-23T19:55:00Z",
            99.0,
        ))
        .unwrap();
    let report = store.event_history(&context).unwrap();
    assert_eq!(report.before.unwrap().underlying.last, Some(99.0));
    assert!(report.after.is_none());
    assert_eq!(report.unclassified_count, 1);
    assert!(options::quote_instant("2026-11-01 01:30:00", "America/New_York").is_none());
    assert_eq!(
        options::quote_instant("2026-09-24 15:55:00", "America/New_York")
            .unwrap()
            .unix_timestamp(),
        options::quote_instant("2026-09-24T19:55:00Z", "UTC")
            .unwrap()
            .unix_timestamp()
    );
}

#[tokio::test]
async fn historical_options_start_and_remain_readable_without_opend() {
    struct Offline;
    #[async_trait::async_trait]
    impl OptionsApi for Offline {
        async fn expirations(
            &self,
            _: &str,
        ) -> tickloop_ticks::error::Result<Vec<options::Expiration>> {
            Err(tickloop_ticks::error::Error::network("OpenD unavailable"))
        }
        async fn chain(&self, _: &str, _: &str) -> tickloop_ticks::error::Result<Vec<Contract>> {
            unreachable!()
        }
        async fn snapshots(&self, _: &[String]) -> tickloop_ticks::error::Result<Vec<MarketQuote>> {
            unreachable!()
        }
    }
    let api = demo_api();
    let template = options::load(
        api.as_ref(),
        &OptionsRequest::new("DEMO.US", None, 21).unwrap(),
    )
    .await
    .unwrap();
    let temporary = tempfile::tempdir().unwrap();
    let store = SnapshotStore::new(Some(temporary.path().into())).unwrap();
    let context = research("FY2026-Q2", "2026-08-06");
    store
        .save(&event_snapshot(
            template,
            &context,
            "2026-08-06T19:55:00Z",
            100.0,
        ))
        .unwrap();
    let mut view = OptionsView::for_research(context, Arc::new(Offline), store).unwrap();
    assert!(!view.table().rows.is_empty());
    assert!(view.content()["current_snapshot"].is_null());
    let mut state = ViewState::default();
    assert!(
        view.on_key(KeyCode::Char('c'), &NoMarketApi, &mut state)
            .await
            .is_err()
    );
    view.on_key(KeyCode::Char('b'), &NoMarketApi, &mut state)
        .await
        .unwrap();
    assert!(!view.table().rows.is_empty());
    for (width, height) in [(48, 16), (120, 30)] {
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, height)).unwrap();
        terminal
            .draw(|frame| view.draw(frame, &mut state, &Settings::default()))
            .unwrap();
        let text = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(text.contains("FY2026-Q2"));
    }
}

fn quote(symbol: &str, bid: f64, ask: f64) -> MarketQuote {
    MarketQuote {
        symbol: symbol.into(),
        last: Some((bid + ask) / 2.0),
        bid: Some(bid),
        ask: Some(ask),
        quote_time: Some("2026-09-24 15:55:00".into()),
        ..Default::default()
    }
}

fn pair(strike: f64) -> StrikePair {
    let leg = |side, name: &str, bid, ask| {
        Some(OptionLeg {
            contract: Contract {
                symbol: name.into(),
                strike,
                expiry: "2026-09-25".into(),
                side,
                standard_type: Some(1),
            },
            quote: Some(quote(name, bid, ask)),
        })
    };
    StrikePair {
        strike,
        call: leg(OptionSide::Call, "C.US", 3.0, 3.2),
        put: leg(OptionSide::Put, "P.US", 2.8, 3.0),
    }
}

#[test]
fn atm_straddle_preserves_method_legs_and_times_without_using_iv_as_move() {
    let underlying = quote("AAPL.US", 99.9, 100.1);
    let estimate =
        options::straddle(&underlying, &[pair(95.0), pair(100.0)], "2026-09-25").unwrap();
    assert!((estimate.move_percent - 6.0).abs() < 1e-9);
    assert_eq!(estimate.strike, 100.0);
    assert_eq!(estimate.call_symbol, "C.US");
    assert_eq!(estimate.call_quote_time, underlying.quote_time);
    assert!(estimate.method.contains("midpoint"));
    assert!(estimate.interpretation.contains("not direction"));
}

#[test]
fn missing_zero_crossed_mixed_expiry_and_nonstandard_legs_do_not_create_an_estimate() {
    let underlying = quote("AAPL.US", 99.9, 100.1);
    for case in 0..7 {
        let mut row = pair(100.0);
        match case {
            0 => row.call.as_mut().unwrap().quote.as_mut().unwrap().bid = None,
            1 => row.call.as_mut().unwrap().quote.as_mut().unwrap().bid = Some(0.0),
            2 => row.call.as_mut().unwrap().quote.as_mut().unwrap().ask = Some(2.0),
            3 => row.put.as_mut().unwrap().contract.expiry = "2026-10-02".into(),
            4 => row.put.as_mut().unwrap().contract.standard_type = Some(2),
            5 => {
                row.call
                    .as_mut()
                    .unwrap()
                    .quote
                    .as_mut()
                    .unwrap()
                    .quote_time = None
            }
            _ => row.put = None,
        }
        assert!(
            options::straddle(&underlying, &[row], "2026-09-25").is_none(),
            "case {case}"
        );
    }
    let mut nearest = pair(100.0);
    nearest.call.as_mut().unwrap().quote = None;
    assert!(options::straddle(&underlying, &[nearest, pair(110.0)], "2026-09-25").is_none());
}

#[test]
fn provider_json_parses_optional_fields_and_string_int64_without_inventing_quotes() {
    let quotes=futu::parse_snapshots(&json!({"snapshotList":[{"basic":{"security":{"market":11,"code":"AAPL260925C100000"},"curPrice":3.0,"volume":"9007199254740993","updateTime":"2026-09-24 15:55:00"},"optionExData":{"impliedVolatility":88.1,"openInterest":"1200"}}]})).unwrap();
    assert_eq!(quotes[0].volume, Some(9_007_199_254_740_993));
    assert_eq!(quotes[0].open_interest, Some(1200));
    assert_eq!(quotes[0].implied_volatility_percent, Some(88.1));
    assert_eq!(quotes[0].bid, None);
    assert_eq!(quotes[0].midpoint(), None);
    assert!(
        futu::parse_snapshots(
            &json!({"snapshotList":[{"basic":{"security":{"market":99,"code":"X"}}}]})
        )
        .is_err()
    );
}

#[test]
fn wire_header_matches_opend_little_endian_format_and_checks_integrity() {
    let body = br#"{"retType":0,"s2c":{}}"#;
    let frame = futu::encode_frame(3224, 7, body).unwrap();
    assert_eq!(
        &frame[..12],
        &[b'F', b'T', 0x98, 0x0c, 0, 0, 1, 0, 7, 0, 0, 0]
    );
    assert_eq!(
        u32::from_le_bytes(frame[12..16].try_into().unwrap()) as usize,
        body.len()
    );
    assert_eq!(&frame[36..44], &[0; 8]);
    let header = frame[..44].try_into().unwrap();
    let (protocol, serial, decoded) = futu::decode_frame(&header, &frame[44..]).unwrap();
    assert_eq!((protocol, serial), (3224, 7));
    assert_eq!(decoded["retType"], 0);
    let mut corrupt = body.to_vec();
    corrupt[1] = b'!';
    assert!(futu::decode_frame(&header, &corrupt).is_err());
}

#[test]
fn option_requests_normalize_broker_notation_and_reject_invalid_dates_or_paths() {
    assert_eq!(
        OptionsRequest::new("US.AAPL", None, 21).unwrap().symbol,
        "AAPL.US"
    );
    assert_eq!(
        OptionsRequest::new("700", None, 21).unwrap().symbol,
        "00700.HK"
    );
    assert!(OptionsRequest::new("../../AAPL", None, 21).is_err());
    assert!(OptionsRequest::new("AAPL", Some("2026-02-30"), 21).is_err());
    assert!(OptionsRequest::new("AAPL", Some("2026-09-25-extra"), 21).is_err());
    assert!(OptionsRequest::new("AAPL", None, 101).is_err());
}

#[tokio::test]
async fn demo_uses_real_assembly_and_history_requires_an_explicit_saved_snapshot() {
    let api = demo_api();
    let request = OptionsRequest::new("DEMO.US", None, 21).unwrap();
    let snapshot = options::load(api.as_ref(), &request).await.unwrap();
    assert_eq!(snapshot.source, "demo");
    assert!((snapshot.atm.as_ref().unwrap().move_percent - 6.0).abs() < 1e-9);
    let temporary = tempfile::tempdir().unwrap();
    let store = SnapshotStore::new(Some(temporary.path().join("history"))).unwrap();
    assert!(store.history("DEMO.US", 20).unwrap().missing);
    let path = store.save(&snapshot).unwrap();
    assert!(path.is_file());
    let history = store.history("DEMO.US", 20).unwrap();
    assert_eq!(
        history.snapshots.as_slice(),
        std::slice::from_ref(&snapshot)
    );
    assert!(!history.missing);
    std::fs::write(
        store
            .directory
            .join("DEMO.US-99999999999999999999-bad.json"),
        "broken",
    )
    .unwrap();
    assert_eq!(store.history("DEMO.US", 20).unwrap().invalid_files.len(), 1);
    assert_eq!(store.history("DEMO.US", 20).unwrap().snapshots, [snapshot]);
}

#[tokio::test]
async fn options_tick_changes_expiry_and_exposes_live_tools_without_a_longbridge_service() {
    let api = demo_api();
    let request = OptionsRequest::new("DEMO.US", None, 21).unwrap();
    let snapshot = options::load(api.as_ref(), &request).await.unwrap();
    let first_expiry = snapshot.expiry.clone();
    let temporary = tempfile::tempdir().unwrap();
    let store = SnapshotStore::new(Some(temporary.path().to_path_buf())).unwrap();
    let mut view = OptionsView::new(request, snapshot, api, store);
    let mut state = ViewState::default();
    assert!(
        view.on_key(KeyCode::Esc, &NoMarketApi, &mut state)
            .await
            .unwrap()
    );
    view.on_key(KeyCode::Char(']'), &NoMarketApi, &mut state)
        .await
        .unwrap();
    assert_ne!(view.content()["expiry"], first_expiry);
    let tools = view.tools();
    assert_eq!(
        tools.catalog(),
        tickloop_ticks::commands::options::tools().catalog()
    );
    let ToolDispatch::Query(query) = tools
        .get("snapshot")
        .unwrap()
        .prepare(&NoMarketApi, json!({}))
        .unwrap()
    else {
        panic!("snapshot must be query")
    };
    assert_eq!(query.await.unwrap(), view.content());
    let saved = view
        .act(ToolCall {
            tool: "save_snapshot".into(),
            args: json!({}),
        })
        .await
        .unwrap();
    assert!(std::path::Path::new(saved["path"].as_str().unwrap()).is_file());
    view.on_key(KeyCode::Char('h'), &NoMarketApi, &mut state)
        .await
        .unwrap();
    assert_eq!(view.table().rows.len(), 1);
    view.on_key(KeyCode::Enter, &NoMarketApi, &mut state)
        .await
        .unwrap();
    for (width, height) in [(70, 14), (120, 28)] {
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, height)).unwrap();
        terminal
            .draw(|frame| view.draw(frame, &mut state, &Settings::default()))
            .unwrap();
        let contents = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(contents.contains("saved snapshot"));
    }
    view.on_key(KeyCode::Esc, &NoMarketApi, &mut state)
        .await
        .unwrap();
    view.on_key(KeyCode::Esc, &NoMarketApi, &mut state)
        .await
        .unwrap();
    assert!(view.table().rows.len() > 1);
}

#[tokio::test]
async fn native_provider_reads_a_mock_opend_server_over_the_actual_tcp_protocol() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = tokio::spawn(async move {
        for connection in 0..4 {
            let (mut socket, _) = listener.accept().await.unwrap();
            for stage in 0..if connection == 0 { 1 } else { 2 } {
                let mut header = [0; 44];
                socket.read_exact(&mut header).await.unwrap();
                let length = u32::from_le_bytes(header[12..16].try_into().unwrap()) as usize;
                let mut body = vec![0; length];
                socket.read_exact(&mut body).await.unwrap();
                let (protocol, serial, request) = futu::decode_frame(&header, &body).unwrap();
                let data = if stage == 0 {
                    assert_eq!(protocol, 1001);
                    assert_eq!(request["c2s"]["pushProtoFmt"], 1);
                    json!({"keepAliveInterval":10})
                } else {
                    match protocol {
                        3224 => {
                            assert_eq!(request["c2s"]["owner"], json!({"market":11,"code":"AAPL"}));
                            json!({"dateList":[{"strikeTime":"2026-09-25","optionExpiryDateDistance":1}]})
                        }
                        3209 => {
                            assert_eq!(request["c2s"]["beginTime"], "2026-09-25");
                            json!({"optionChain":[{"strikeTime":"2026-09-25","option":[{"call":{"basic":{"security":{"market":11,"code":"AAPL260925C100000"}},"optionExData":{"strikeTime":"2026-09-25","strikePrice":100,"optionStandardType":1}}}]}]})
                        }
                        3203 => {
                            assert_eq!(request["c2s"]["securityList"][0]["code"], "AAPL");
                            json!({"snapshotList":[{"basic":{"security":{"market":11,"code":"AAPL"},"curPrice":100,"bidPrice":99.9,"askPrice":100.1,"updateTime":"2026-09-24 15:55:00","volume":"1000"}}]})
                        }
                        _ => panic!("unexpected protocol {protocol}"),
                    }
                };
                let response = serde_json::to_vec(&json!({"retType":0,"s2c":data})).unwrap();
                socket
                    .write_all(&futu::encode_frame(protocol, serial, &response).unwrap())
                    .await
                    .unwrap();
            }
        }
    });
    let api = Arc::new(futu::FutuOptions::connect("127.0.0.1", port).await.unwrap());
    assert_eq!(
        api.expirations("AAPL.US").await.unwrap()[0].date,
        "2026-09-25"
    );
    assert_eq!(
        api.chain("AAPL.US", "2026-09-25").await.unwrap()[0].strike,
        100.0
    );
    assert_eq!(
        api.snapshots(&["AAPL.US".into()]).await.unwrap()[0].last,
        Some(100.0)
    );
    server.await.unwrap();
}
