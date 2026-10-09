use std::path::PathBuf;

use serde_json::json;
use tick_connect::protocol::descriptor_digest;
use tick_connect::session::{
    SessionClient, SessionError, SessionLaunch, SessionLinkState, TickSession,
};
use tokio::io::AsyncWriteExt;
use tokio_util::sync::CancellationToken;

async fn counter() -> (TickSession, tempfile::TempDir) {
    let directory = tempfile::tempdir().unwrap();
    let script = directory.path().join("counter.py");
    std::fs::write(&script, r#"
import json, os, sys, time
instance = os.environ['TICKLOOP_INSTANCE_ID']
value = 0
for line in sys.stdin:
    request = json.loads(line)
    method = request['method']
    if method == 'tick/describe':
        result = {'protocol':2,'tick_type':'example.counter','title':'Counter','description':'External counter','recover_with':'current','tools':[{'name':'increment','description':'Increment','params':{'type':'object'},'mutates':True},{'name':'current','description':'Read','params':{'type':'object'},'mutates':False}]}
    elif method == 'tick/read':
        result = {'version':3,'tick_instance_id':instance,'initialized':True,'sequence':value,'observed_at':'2026-09-24T00:00:00Z','data':{'value':value}}
    elif method == 'tool/call':
        if request['params']['tool'] == 'increment':
            value += 1
            print(json.dumps({'jsonrpc':'2.0','method':'tick/update','params':{'version':3,'tick_instance_id':instance,'source':'example.counter','sequence':value,'observed_at':'2026-09-24T00:00:01Z','data':{'value':value}}}),flush=True)
            time.sleep(0.1)
        result = {'value':value}
    else:
        raise RuntimeError(method)
    print(json.dumps({'jsonrpc':'2.0','id':request['id'],'result':result}),flush=True)
"#).unwrap();
    let session = TickSession::spawn(SessionLaunch {
        command: vec![
            "python3".into(),
            "-u".into(),
            script.to_string_lossy().into_owned(),
        ],
        cwd: directory.path().into(),
        backend: tick_connect::session::SessionBackend::Herdr {
            socket: PathBuf::from("/tmp/example-herdr.sock"),
            terminal_id: "counter-pane".into(),
        },
        runtime_dir: Some(directory.path().join("sessions")),
    })
    .await
    .unwrap();
    (session, directory)
}

#[tokio::test]
async fn later_subscriber_gets_its_own_baseline_and_actual_received_count() {
    let (session, directory) = counter().await;
    let client = SessionClient::connect(
        session.instance().clone(),
        tick_connect::session::SessionBackend::Herdr {
            socket: PathBuf::from("/tmp/example-herdr.sock"),
            terminal_id: "counter-pane".into(),
        },
        Some(directory.path().join("sessions")),
    )
    .await
    .unwrap();
    let (early_read, early_received, mut early) = client.watch().await.unwrap();
    assert_eq!(early_read.data, Some(json!({"value":0})));
    assert_eq!(early_received, 0);
    client
        .call("increment", json!({}), CancellationToken::new())
        .await
        .unwrap();
    early.changed().await.unwrap();
    assert_eq!(early.borrow_and_update().received, 1);
    assert_eq!(
        early.borrow().update.as_ref().unwrap().data,
        json!({"value":1})
    );

    let (late_read, late_received, mut late) = client.watch().await.unwrap();
    assert_eq!(late_read.data, Some(json!({"value":1})));
    assert_eq!(late_received, 1);
    client
        .call("increment", json!({}), CancellationToken::new())
        .await
        .unwrap();
    early.changed().await.unwrap();
    late.changed().await.unwrap();
    assert_eq!(early.borrow_and_update().received, 2);
    assert_eq!(late.borrow_and_update().received, 2);
    assert_eq!(
        late.borrow().update.as_ref().unwrap().data,
        json!({"value":2})
    );
    session.shutdown().await.unwrap();
}

#[tokio::test]
async fn slow_subscriber_sees_latest_content_and_all_received_frames_counted() {
    let (session, _directory) = counter().await;
    let client = session.client();
    let (_, registration_received, mut observations) = client.watch().await.unwrap();
    assert_eq!(registration_received, 0);
    for _ in 0..3 {
        client
            .call("increment", json!({}), CancellationToken::new())
            .await
            .unwrap();
    }
    observations.changed().await.unwrap();
    let latest = (*observations.borrow_and_update()).clone();
    assert_eq!(latest.received, 3);
    assert_eq!(latest.update.unwrap().data, json!({"value":3}));
    session.shutdown().await.unwrap();
}

#[tokio::test]
async fn concurrent_clients_with_identical_calls_receive_their_own_responses() {
    let (session, directory) = counter().await;
    let first = SessionClient::connect(
        session.instance().clone(),
        tick_connect::session::SessionBackend::Herdr {
            socket: PathBuf::from("/tmp/example-herdr.sock"),
            terminal_id: "counter-pane".into(),
        },
        Some(directory.path().join("sessions")),
    )
    .await
    .unwrap();
    let second = SessionClient::connect(
        session.instance().clone(),
        tick_connect::session::SessionBackend::Herdr {
            socket: PathBuf::from("/tmp/example-herdr.sock"),
            terminal_id: "counter-pane".into(),
        },
        Some(directory.path().join("sessions")),
    )
    .await
    .unwrap();
    let (first_result, second_result) = tokio::join!(
        first.call("increment", json!({}), CancellationToken::new()),
        second.call("increment", json!({}), CancellationToken::new())
    );
    let mut values = [
        first_result.unwrap()["value"].as_u64().unwrap(),
        second_result.unwrap()["value"].as_u64().unwrap(),
    ];
    values.sort_unstable();
    assert_eq!(values, [1, 2]);
    assert_eq!(
        session.client().read().await.unwrap().data,
        Some(json!({"value":2}))
    );
    session.shutdown().await.unwrap();
}

#[tokio::test]
async fn closing_one_in_flight_client_does_not_cancel_a_peers_call() {
    let (session, _directory) = counter().await;
    let departing = session.client();
    let peer = session.client();
    let (_, _, mut observations) = session.client().watch().await.unwrap();
    let cancellation = CancellationToken::new();
    let called = cancellation.clone();
    let first_call =
        tokio::spawn(async move { departing.call("increment", json!({}), called).await });
    observations.changed().await.unwrap();
    assert_eq!(observations.borrow_and_update().received, 1);
    cancellation.cancel();
    assert!(matches!(
        first_call.await.unwrap(),
        Err(SessionError::OutcomeUnknown)
    ));
    assert_eq!(
        peer.call("increment", json!({}), CancellationToken::new())
            .await
            .unwrap(),
        json!({"value":2})
    );
    assert_eq!(peer.read().await.unwrap().data, Some(json!({"value":2})));
    session.shutdown().await.unwrap();
}

#[tokio::test]
async fn sustained_updates_with_an_unread_watcher_do_not_block_peer_reads_or_tools() {
    let (session, _directory) = counter().await;
    let producer = session.client();
    let peer = session.client();
    let (_, registration_received, mut slow_watcher) = session.client().watch().await.unwrap();
    assert_eq!(registration_received, 0);
    let produce = async {
        for _ in 0..16 {
            producer
                .call("increment", json!({}), CancellationToken::new())
                .await
                .unwrap();
        }
    };
    let use_peer = async {
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
        let before = peer.read().await.unwrap().data.unwrap()["value"]
            .as_u64()
            .unwrap();
        let result = peer
            .call("increment", json!({}), CancellationToken::new())
            .await
            .unwrap();
        assert!(result["value"].as_u64().unwrap() > before);
    };
    tokio::join!(produce, use_peer);
    assert_eq!(peer.read().await.unwrap().data, Some(json!({"value":17})));
    let latest = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            slow_watcher.changed().await.unwrap();
            let observation = slow_watcher.borrow_and_update().clone();
            if observation.received == 17 {
                return observation;
            }
        }
    })
    .await
    .unwrap();
    assert_eq!(latest.update.unwrap().data, json!({"value":17}));
    session.shutdown().await.unwrap();
}

#[tokio::test]
async fn cancellation_after_update_reports_unknown_and_does_not_retry() {
    let (session, _directory) = counter().await;
    let client = session.client();
    let (_, _, mut observations) = client.watch().await.unwrap();
    let cancellation = CancellationToken::new();
    let called = cancellation.clone();
    let caller = tokio::spawn(async move { client.call("increment", json!({}), called).await });
    observations.changed().await.unwrap();
    assert_eq!(
        observations
            .borrow_and_update()
            .update
            .as_ref()
            .unwrap()
            .data,
        json!({"value":1})
    );
    cancellation.cancel();
    assert!(matches!(
        caller.await.unwrap(),
        Err(SessionError::OutcomeUnknown)
    ));
    assert_eq!(
        session.client().read().await.unwrap().data,
        Some(json!({"value":1}))
    );
    session.shutdown().await.unwrap();
}

#[tokio::test]
async fn oversized_request_is_rejected_before_the_child_receives_it() {
    let (session, _directory) = counter().await;
    let cancelled = CancellationToken::new();
    cancelled.cancel();
    assert!(matches!(
        session
            .client()
            .call("increment", json!({}), cancelled)
            .await,
        Err(SessionError::CancelledBeforeSend)
    ));
    let result = session
        .client()
        .call(
            "increment",
            json!({"padding":"x".repeat(600_000)}),
            CancellationToken::new(),
        )
        .await;
    assert!(
        matches!(result, Err(SessionError::Protocol(message)) if message.contains("too large"))
    );
    assert_eq!(
        session.client().read().await.unwrap().data,
        Some(json!({"value":0}))
    );
    session.shutdown().await.unwrap();
}

#[tokio::test]
async fn prepared_call_rechecks_catalog_before_dispatch() {
    let (session, _directory) = counter().await;
    let client = session.client();
    let descriptor = client.describe().await.unwrap();
    let stale = client
        .call_prepared(
            "increment",
            json!({}),
            "stale-catalog",
            CancellationToken::new(),
        )
        .await;
    assert!(
        matches!(stale, Err(SessionError::Protocol(message)) if message.contains("catalog changed"))
    );
    assert_eq!(client.read().await.unwrap().data, Some(json!({"value":0})));
    client
        .call_prepared(
            "increment",
            json!({}),
            &descriptor_digest(&descriptor),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(client.read().await.unwrap().data, Some(json!({"value":1})));
    session.shutdown().await.unwrap();
}

#[tokio::test]
async fn shutdown_does_not_remove_a_replacement_at_the_endpoint_path() {
    let (session, _directory) = counter().await;
    let endpoint = session.endpoint().to_path_buf();
    let original = endpoint.with_extension("old");
    std::fs::rename(&endpoint, &original).unwrap();
    std::fs::write(&endpoint, "another resource").unwrap();
    session.shutdown().await.unwrap();
    assert_eq!(
        std::fs::read_to_string(&endpoint).unwrap(),
        "another resource"
    );
    std::fs::remove_file(endpoint).unwrap();
    std::fs::remove_file(original).unwrap();
}

#[tokio::test]
async fn malformed_child_response_after_dispatch_is_unknown() {
    let directory = tempfile::tempdir().unwrap();
    let script = directory.path().join("broken.py");
    std::fs::write(&script, r#"
import json, os, sys
instance = os.environ['TICKLOOP_INSTANCE_ID']
for line in sys.stdin:
    request = json.loads(line)
    if request['method'] == 'tick/describe':
        result = {'protocol':2,'tick_type':'example.broken','title':'Broken','description':'Fixture','recover_with':'current','tools':[{'name':'increment','description':'Increment','params':{'type':'object'},'mutates':True},{'name':'current','description':'Read','params':{'type':'object'},'mutates':False}]}
    elif request['method'] == 'tick/read':
        result = {'version':3,'tick_instance_id':instance,'initialized':True,'sequence':0,'observed_at':'2026-09-24T00:00:00Z','data':{'value':0}}
    else:
        print('not-json', flush=True)
        break
    print(json.dumps({'jsonrpc':'2.0','id':request['id'],'result':result}), flush=True)
"#).unwrap();
    let session = TickSession::spawn(SessionLaunch {
        command: vec![
            "python3".into(),
            "-u".into(),
            script.to_string_lossy().into_owned(),
        ],
        cwd: directory.path().into(),
        backend: tick_connect::session::SessionBackend::Herdr {
            socket: PathBuf::from("/tmp/example-herdr.sock"),
            terminal_id: "broken-pane".into(),
        },
        runtime_dir: Some(directory.path().join("sessions")),
    })
    .await
    .unwrap();
    let result = session
        .client()
        .call("increment", json!({}), CancellationToken::new())
        .await;
    assert!(matches!(result, Err(SessionError::OutcomeUnknown)));
    session.shutdown().await.unwrap_or_else(|error| {
        assert!(matches!(error, SessionError::Io(_)));
    });
}

#[tokio::test]
async fn ninth_connection_is_busy_without_dispatching_an_action() {
    let (session, _directory) = counter().await;
    let client = session.client();
    let mut watchers = Vec::new();
    for _ in 0..8 {
        let (_, _, receiver) = client.watch().await.unwrap();
        watchers.push(receiver);
    }
    let result = client
        .call("increment", json!({}), CancellationToken::new())
        .await;
    assert!(matches!(result, Err(SessionError::Busy)));
    drop(watchers);
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    assert_eq!(client.read().await.unwrap().data, Some(json!({"value":0})));
    session.shutdown().await.unwrap();
}

#[tokio::test]
async fn client_binding_rejects_a_different_host_or_terminal() {
    let (session, directory) = counter().await;
    let wrong_host = SessionClient::connect(
        session.instance().clone(),
        tick_connect::session::SessionBackend::Herdr {
            socket: PathBuf::from("/tmp/wrong-herdr.sock"),
            terminal_id: "counter-pane".into(),
        },
        Some(directory.path().join("sessions")),
    )
    .await;
    assert!(matches!(wrong_host, Err(SessionError::Protocol(_))));
    let wrong_terminal = SessionClient::connect(
        session.instance().clone(),
        tick_connect::session::SessionBackend::Herdr {
            socket: PathBuf::from("/tmp/example-herdr.sock"),
            terminal_id: "other-pane".into(),
        },
        Some(directory.path().join("sessions")),
    )
    .await;
    assert!(matches!(wrong_terminal, Err(SessionError::Protocol(_))));
    assert_eq!(
        session.client().read().await.unwrap().data,
        Some(json!({"value":0}))
    );
    session.shutdown().await.unwrap();
}

#[tokio::test]
async fn oversized_child_response_is_never_reported_as_success() {
    let directory = tempfile::tempdir().unwrap();
    let script = directory.path().join("oversized.py");
    std::fs::write(&script, r#"
import json, os, sys
instance = os.environ['TICKLOOP_INSTANCE_ID']
for line in sys.stdin:
    request = json.loads(line)
    if request['method'] == 'tick/describe':
        result = {'protocol':2,'tick_type':'example.large','title':'Large','description':'Fixture','recover_with':'current','tools':[{'name':'large','description':'Large','params':{'type':'object'},'mutates':True},{'name':'current','description':'Read','params':{'type':'object'},'mutates':False}]}
    elif request['method'] == 'tick/read':
        result = {'version':3,'tick_instance_id':instance,'initialized':True,'sequence':0,'observed_at':'2026-09-24T00:00:00Z','data':{'value':0}}
    else:
        result = {'text':'x' * (8 * 1024 * 1024)}
    print(json.dumps({'jsonrpc':'2.0','id':request['id'],'result':result}), flush=True)
"#).unwrap();
    let session = TickSession::spawn(SessionLaunch {
        command: vec![
            "python3".into(),
            "-u".into(),
            script.to_string_lossy().into_owned(),
        ],
        cwd: directory.path().into(),
        backend: tick_connect::session::SessionBackend::Herdr {
            socket: PathBuf::from("/tmp/example-herdr.sock"),
            terminal_id: "large-pane".into(),
        },
        runtime_dir: Some(directory.path().join("sessions")),
    })
    .await
    .unwrap();
    let result = session
        .client()
        .call("large", json!({}), CancellationToken::new())
        .await;
    assert!(matches!(result, Err(SessionError::OutcomeUnknown)));
    let _ = session.shutdown().await;
}

#[tokio::test]
async fn child_that_stops_reading_stdin_expires_and_exits_instead_of_hanging_session() {
    let directory = tempfile::tempdir().unwrap();
    let script = directory.path().join("nonreader.py");
    std::fs::write(&script, r#"
import json, os, sys, time
instance = os.environ['TICKLOOP_INSTANCE_ID']
for line in sys.stdin:
    request = json.loads(line)
    if request['method'] == 'tick/describe':
        result = {'protocol':2,'tick_type':'example.nonreader','title':'Nonreader','description':'Fixture','recover_with':None,'tools':[]}
    elif request['method'] == 'tick/read':
        result = {'version':3,'tick_instance_id':instance,'initialized':True,'sequence':0,'observed_at':'2026-09-24T00:00:00Z','data':{}}
    else:
        raise RuntimeError('unexpected request')
    print(json.dumps({'jsonrpc':'2.0','id':request['id'],'result':result}), flush=True)
    if request['method'] == 'tick/read':
        time.sleep(60)
"#).unwrap();
    let session = TickSession::spawn(SessionLaunch {
        command: vec![
            "python3".into(),
            "-u".into(),
            script.to_string_lossy().into_owned(),
        ],
        cwd: directory.path().into(),
        backend: tick_connect::session::SessionBackend::Herdr {
            socket: PathBuf::from("/tmp/example-herdr.sock"),
            terminal_id: "nonreader-pane".into(),
        },
        runtime_dir: Some(directory.path().join("sessions")),
    })
    .await
    .unwrap();
    let (_, _, mut observations) = session.client().watch().await.unwrap();
    let mut stream = tokio::net::UnixStream::connect(session.endpoint())
        .await
        .unwrap();
    let request = json!({
        "jsonrpc":"2.0", "id":1, "method":"tick/describe",
        "binding":{"instance_id":session.instance().to_string(),"backend":{"kind":"herdr","socket":"/tmp/example-herdr.sock","terminal_id":"nonreader-pane"}},
        "params":{"padding":"x".repeat(400_000)}
    });
    stream
        .write_all(format!("{request}\n").as_bytes())
        .await
        .unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(15), observations.changed())
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(
        observations.borrow_and_update().state,
        SessionLinkState::ProtocolError(_)
    ));
    let status = tokio::time::timeout(std::time::Duration::from_secs(5), session.wait())
        .await
        .unwrap()
        .unwrap();
    assert!(!status.success());
}
