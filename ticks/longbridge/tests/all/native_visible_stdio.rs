#![cfg(all(unix, feature = "live-api"))]

use std::{
    io::Write,
    process::Command,
    time::{Duration, Instant},
};

use serde_json::json;

use crate::support::hosted::{self, Hosted};

/// Live candles take a quote connection and a history request before the first update.
const FRAME_TIMEOUT: Duration = Duration::from_secs(30);

/// `tick-longbridge` as its host launches it. It reads the login in the caller's own home.
fn hosted_longbridge(arguments: &[&str]) -> Hosted {
    let mut command = Command::new(env!("CARGO_BIN_EXE_tick-longbridge"));
    command
        .args(arguments)
        .env("TICKLOOP_INSTANCE_ID", "hosted:live-kline");
    hosted::launch(command)
}

/// Run in your own terminal after `tick-longbridge auth`:
/// `cargo nextest run -p tickloop-tick-longbridge --features live-api --run-ignored only`.
#[test]
#[ignore = "reads live Longbridge candles with your own login"]
fn a_hosted_kline_describes_reads_calls_and_updates_over_protocol_3() {
    let mut tick = hosted_longbridge(&["kline", "700.HK"]);

    let first = tick.frame_where(FRAME_TIMEOUT, |frame| {
        frame["method"] == "tick/update" && frame["params"]["data"]["symbol"].is_string()
    });
    assert_eq!(first["params"]["data"]["symbol"], "700.HK");

    tick.request(1, "tick/describe", json!({}));
    let describe = tick.frame_where(FRAME_TIMEOUT, |frame| frame["id"] == 1);
    assert_eq!(describe["result"]["protocol"], 3);
    assert_eq!(describe["result"]["kind"], "kline");
    let revision = describe["result"]["revision"].clone();

    tick.request(2, "tick/read", json!({}));
    let read = tick.frame_where(FRAME_TIMEOUT, |frame| frame["id"] == 2);
    assert_eq!(read["result"]["data"]["period"], "day");

    tick.request(
        3,
        "tick/call",
        json!({"tool":"mark","args":{"label":"live","price":400.0},"revision":revision}),
    );
    let marked = tick.frame_where(FRAME_TIMEOUT, |frame| {
        frame["method"] == "tick/update"
            && frame["params"]["data"]["marks"]
                .as_array()
                .is_some_and(|marks| !marks.is_empty())
    });
    assert_eq!(marked["params"]["data"]["marks"][0]["label"], "live");

    tick.tty.write_all(b"q").unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while tick.child.try_wait().unwrap().is_none() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    if tick.child.try_wait().unwrap().is_none() {
        tick.child.kill().unwrap();
        panic!("tick-longbridge did not exit after q");
    }
    assert!(tick.child.wait().unwrap().success());
}
