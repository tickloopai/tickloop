#![cfg(unix)]
//! A real `tick` opened hidden by the Tickhost library: it connects itself through the hosted
//! launch contract, without Herdr or `tickhost open`.

use std::time::Duration;

use serde_json::{Value, json};
use tickhost::{host, paths::HostPaths};
use tickhost_connect::hosted::{
    CAPABILITY_DATA, CAPABILITY_LIFECYCLE, CONTROL_TIMEOUT, HostConnection, Role,
};

#[tokio::test(flavor = "multi_thread")]
async fn a_hidden_builtin_watchlist_runs_on_a_private_pty_without_herdr() {
    let directory = tempfile::Builder::new()
        .prefix("thb")
        .tempdir_in("/private/tmp")
        .unwrap();
    let paths = HostPaths {
        runtime: directory.path().join("h"),
    };
    let key = host::random_token();
    let server = tokio::spawn(host::serve(
        paths.clone(),
        host::HostConfig {
            controller_key: key.clone(),
            idle_timeout: host::DEFAULT_IDLE_TIMEOUT,
        },
    ));
    let controller_role = Role::Controller {
        controller_key: key,
        daemon_epoch: uuid::Uuid::new_v4(),
    };
    // The socket file appears when the host binds, before it listens; only an accepted
    // connection shows the host is serving.
    let deadline = tokio::time::Instant::now() + CONTROL_TIMEOUT;
    let mut controller = loop {
        match HostConnection::connect(
            &paths.socket(),
            controller_role.clone(),
            &[CAPABILITY_LIFECYCLE],
        )
        .await
        {
            Ok(connection) => break connection,
            Err(_) if tokio::time::Instant::now() < deadline => {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            Err(error) => panic!("the host never accepted a controller: {error}"),
        }
    };
    let owner = json!({"root":"fixture-root","chat":uuid::Uuid::new_v4()});
    let opened: Value = controller
        .request(
            "open",
            json!({
                "operation": uuid::Uuid::new_v4(),
                "owner": owner,
                "owner_revision": 0,
                "adapter": "protocol",
                "command": [env!("CARGO_BIN_EXE_tick"), "builtin", "watchlist"],
                "cwd": directory.path(),
                "environment": std::env::vars().collect::<Vec<_>>(),
                "title": null,
            }),
            CONTROL_TIMEOUT + Duration::from_secs(2),
        )
        .await
        .unwrap();
    assert!(opened["instance"].as_str().unwrap().starts_with("hosted:"));
    let mut reader = HostConnection::connect(
        &paths.socket(),
        Role::Instance {
            instance: serde_json::from_value(opened["instance"].clone()).unwrap(),
            data_token: opened["data_token"].as_str().unwrap().into(),
        },
        &[CAPABILITY_DATA],
    )
    .await
    .unwrap();
    let descriptor = reader
        .request("describe", json!({}), CONTROL_TIMEOUT)
        .await
        .unwrap();
    assert_eq!(descriptor["protocol"], 3);
    assert_eq!(descriptor["kind"], "watchlist");
    assert_eq!(descriptor["revision"], "1");
    let read = reader
        .request("read", json!({"activity":"explicit"}), CONTROL_TIMEOUT)
        .await
        .unwrap();
    assert!(read["seq"].is_u64(), "{read}");
    assert!(
        read["at"].is_string(),
        "the host fills in when it received the state: {read}"
    );
    let business = opened["business_pid"].as_u64().unwrap() as libc::pid_t;
    let closed = controller
        .request(
            "close",
            json!({"operation":uuid::Uuid::new_v4(),"owner":owner,"instance":opened["instance"]}),
            CONTROL_TIMEOUT,
        )
        .await
        .unwrap();
    assert_eq!(closed["state"], "closed");
    let mut ended = false;
    for _ in 0..400 {
        if unsafe { libc::kill(business, 0) } != 0 {
            ended = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert!(ended, "close ends the business process");
    tokio::time::timeout(Duration::from_secs(5), server)
        .await
        .expect("the host exits after its last Tick ends")
        .unwrap()
        .unwrap();
}
