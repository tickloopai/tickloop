use std::path::{Path, PathBuf};

use serde_json::json;
use tickhost_connect::session::{
    SessionBackend, SessionClient, SessionError, SessionLaunch, SessionLinkState, TickSession,
};
use tokio::io::AsyncWriteExt;
use tokio_util::sync::CancellationToken;

const HERDR_SOCKET: &str = "/tmp/example-herdr.sock";
const COUNTER_PANE: &str = "counter-pane";

/// Publishes an update for each `increment` shortly before answering it.
const COUNTER: &str = r#"
import json, sys, time
value = 0
for line in sys.stdin:
    request = json.loads(line)
    method = request['method']
    if method == 'tick/describe':
        result = {'protocol':3,'revision':'1','kind':'example.counter','title':'Counter','description':'External counter','recover_with':'current','tools':[{'name':'increment','description':'Increment','input_schema':{'type':'object'},'mutates':True},{'name':'current','description':'Read','input_schema':{'type':'object'},'mutates':False}]}
    elif method == 'tick/read':
        result = {'seq':value,'at':'2026-09-24T00:00:00Z','data':{'value':value}}
    elif request['params'].get('revision') != '1':
        print(json.dumps({'jsonrpc':'2.0','id':request['id'],'error':{'code':-32004,'message':'catalog changed'}}),flush=True)
        continue
    elif method == 'tick/call':
        if request['params']['tool'] == 'increment':
            value += 1
            print(json.dumps({'jsonrpc':'2.0','method':'tick/update','params':{'seq':value,'at':'2026-09-24T00:00:01Z','data':{'value':value}}}),flush=True)
            time.sleep(0.1)
        result = {'value':value}
    else:
        raise RuntimeError(method)
    print(json.dumps({'jsonrpc':'2.0','id':request['id'],'result':result}),flush=True)
"#;

fn herdr_backend(terminal_id: &str) -> SessionBackend {
    SessionBackend::Herdr {
        socket: PathBuf::from(HERDR_SOCKET),
        terminal_id: terminal_id.into(),
    }
}

/// Runs the Python Tick `source` as `{name}.py` in `directory`, in the Herdr pane `{name}-pane`.
async fn spawn_tick(directory: &Path, name: &str, source: &str) -> TickSession {
    let script = directory.join(format!("{name}.py"));
    std::fs::write(&script, source).unwrap();
    TickSession::spawn(SessionLaunch {
        command: vec![
            "python3".into(),
            "-u".into(),
            script.to_string_lossy().into_owned(),
        ],
        cwd: directory.into(),
        backend: herdr_backend(&format!("{name}-pane")),
        runtime_dir: Some(directory.join("sessions")),
    })
    .await
    .unwrap()
}

async fn counter() -> (TickSession, tempfile::TempDir) {
    let directory = tempfile::tempdir().unwrap();
    let session = spawn_tick(directory.path(), "counter", COUNTER).await;
    (session, directory)
}

#[tokio::test]
async fn later_subscriber_gets_its_own_baseline_and_actual_received_count() {
    let (session, directory) = counter().await;
    let client = SessionClient::connect(
        session.instance().clone(),
        herdr_backend(COUNTER_PANE),
        Some(directory.path().join("sessions")),
    )
    .await
    .unwrap();
    let (early_read, early_received, mut early) = client.watch().await.unwrap();
    assert_eq!(early_read.data, json!({"value":0}));
    assert_eq!(early_received, 0);
    client
        .call("increment", json!({}), "1", CancellationToken::new())
        .await
        .unwrap();
    early.changed().await.unwrap();
    assert_eq!(early.borrow_and_update().received, 1);
    assert_eq!(
        early.borrow().update.as_ref().unwrap().data,
        json!({"value":1})
    );

    let (late_read, late_received, mut late) = client.watch().await.unwrap();
    assert_eq!(late_read.data, json!({"value":1}));
    assert_eq!(late_received, 1);
    client
        .call("increment", json!({}), "1", CancellationToken::new())
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
async fn status_reports_the_latest_update_time_without_a_watcher_or_its_content() {
    let (session, _directory) = counter().await;
    let client = session.client();
    let before = client.status().await.unwrap();
    assert_eq!(before.updated_at_ms, None);
    assert_eq!(before.received, 0);
    let called = tickhost_connect::access::now_ms();
    client
        .call("increment", json!({}), "1", CancellationToken::new())
        .await
        .unwrap();
    let after = client.status().await.unwrap();
    assert_eq!(after.received, 1);
    assert!(
        after.updated_at_ms.is_some_and(|at| at >= called),
        "{after:?}"
    );
    let wire = serde_json::to_value(after).unwrap();
    let mut fields: Vec<_> = wire.as_object().unwrap().keys().collect();
    fields.sort();
    assert_eq!(
        fields,
        ["ignored_notifications", "received", "updated_at_ms"],
        "status carries no update content: {wire}"
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
            .call("increment", json!({}), "1", CancellationToken::new())
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
        herdr_backend(COUNTER_PANE),
        Some(directory.path().join("sessions")),
    )
    .await
    .unwrap();
    let second = SessionClient::connect(
        session.instance().clone(),
        herdr_backend(COUNTER_PANE),
        Some(directory.path().join("sessions")),
    )
    .await
    .unwrap();
    let (first_result, second_result) = tokio::join!(
        first.call("increment", json!({}), "1", CancellationToken::new()),
        second.call("increment", json!({}), "1", CancellationToken::new())
    );
    let mut values = [
        first_result.unwrap()["value"].as_u64().unwrap(),
        second_result.unwrap()["value"].as_u64().unwrap(),
    ];
    values.sort_unstable();
    assert_eq!(values, [1, 2]);
    assert_eq!(
        session.client().read().await.unwrap().data,
        json!({"value":2})
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
        tokio::spawn(async move { departing.call("increment", json!({}), "1", called).await });
    observations.changed().await.unwrap();
    assert_eq!(observations.borrow_and_update().received, 1);
    cancellation.cancel();
    assert!(matches!(
        first_call.await.unwrap(),
        Err(SessionError::OutcomeUnknown)
    ));
    assert_eq!(
        peer.call("increment", json!({}), "1", CancellationToken::new())
            .await
            .unwrap(),
        json!({"value":2})
    );
    assert_eq!(peer.read().await.unwrap().data, json!({"value":2}));
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
                .call("increment", json!({}), "1", CancellationToken::new())
                .await
                .unwrap();
        }
    };
    let use_peer = async {
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
        let before = peer.read().await.unwrap().data["value"].as_u64().unwrap();
        let result = peer
            .call("increment", json!({}), "1", CancellationToken::new())
            .await
            .unwrap();
        assert!(result["value"].as_u64().unwrap() > before);
    };
    tokio::join!(produce, use_peer);
    assert_eq!(peer.read().await.unwrap().data, json!({"value":17}));
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
    let caller =
        tokio::spawn(async move { client.call("increment", json!({}), "1", called).await });
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
        json!({"value":1})
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
            .call("increment", json!({}), "1", cancelled)
            .await,
        Err(SessionError::CancelledBeforeSend)
    ));
    let result = session
        .client()
        .call(
            "increment",
            json!({"padding":"x".repeat(600_000)}),
            "1",
            CancellationToken::new(),
        )
        .await;
    assert!(
        matches!(result, Err(SessionError::Protocol(message)) if message.contains("too large"))
    );
    assert_eq!(
        session.client().read().await.unwrap().data,
        json!({"value":0})
    );
    session.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_call_naming_another_revision_is_refused_before_it_runs() {
    let (session, _directory) = counter().await;
    let client = session.client();
    let descriptor = client.describe().await.unwrap();
    let stale = client
        .call(
            "increment",
            json!({}),
            "stale-catalog",
            CancellationToken::new(),
        )
        .await;
    assert!(
        matches!(stale, Err(SessionError::RevisionMismatch)),
        "{stale:?}"
    );
    assert_eq!(client.read().await.unwrap().data, json!({"value":0}));
    client
        .call(
            "increment",
            json!({}),
            &descriptor.revision,
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(client.read().await.unwrap().data, json!({"value":1}));
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
    let session = spawn_tick(
        directory.path(),
        "broken",
        r#"
import json, sys
for line in sys.stdin:
    request = json.loads(line)
    if request['method'] == 'tick/describe':
        result = {'protocol':3,'revision':'1','kind':'example.broken','title':'Broken','description':'Fixture','recover_with':'current','tools':[{'name':'increment','description':'Increment','input_schema':{'type':'object'},'mutates':True},{'name':'current','description':'Read','input_schema':{'type':'object'},'mutates':False}]}
    elif request['method'] == 'tick/read':
        result = {'seq':0,'at':'2026-09-24T00:00:00Z','data':{'value':0}}
    else:
        print('not-json', flush=True)
        break
    print(json.dumps({'jsonrpc':'2.0','id':request['id'],'result':result}), flush=True)
"#,
    )
    .await;
    let result = session
        .client()
        .call("increment", json!({}), "1", CancellationToken::new())
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
        .call("increment", json!({}), "1", CancellationToken::new())
        .await;
    assert!(matches!(result, Err(SessionError::Busy)));
    drop(watchers);
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    assert_eq!(client.read().await.unwrap().data, json!({"value":0}));
    session.shutdown().await.unwrap();
}

/// A Tick that holds its first eight `hold` calls until a `release` file appears, then answers
/// each with a response of almost the 8 MiB limit.
const HOLDING_TICK: &str = r#"
import json, os, sys, time
held = []
calls = 0
for line in sys.stdin:
    request = json.loads(line)
    method = request['method']
    if method == 'tick/describe':
        result = {'protocol':3,'revision':'1','kind':'example.holding','title':'Holding','description':'Fixture','recover_with':'calls','tools':[{'name':'hold','description':'Hold','input_schema':{'type':'object'},'mutates':False},{'name':'calls','description':'Count','input_schema':{'type':'object'},'mutates':False}]}
    elif method == 'tick/read':
        result = {'seq':0,'at':'2026-10-08T00:00:00Z','data':{}}
    elif request['params']['tool'] == 'hold':
        calls += 1
        held.append(request['id'])
        open('received', 'w').write(str(calls))
        if len(held) < 8:
            continue
        while not os.path.exists('release'):
            time.sleep(0.02)
        for id in held:
            print(json.dumps({'jsonrpc':'2.0','id':id,'result':{'payload':'x' * (8 * 1024 * 1024 - 4096)}}), flush=True)
        held = []
        continue
    else:
        result = {'calls': calls}
    print(json.dumps({'jsonrpc':'2.0','id':request['id'],'result':result}), flush=True)
"#;

/// Eight calls in flight hold all eight connections and, with answers of almost 8 MiB each,
/// the whole 64 MiB response budget; a ninth call is Busy before it reaches the Tick, and the
/// eight answers all arrive.
#[tokio::test(flavor = "multi_thread")]
async fn a_ninth_call_in_flight_is_busy_before_dispatch_while_eight_fill_the_response_budget() {
    let directory = tempfile::tempdir().unwrap();
    let session = spawn_tick(directory.path(), "holding", HOLDING_TICK).await;
    let mut held = tokio::task::JoinSet::new();
    for _ in 0..8 {
        let client = session.client();
        held.spawn(async move {
            client
                .call("hold", json!({}), "1", CancellationToken::new())
                .await
        });
    }
    let received = directory.path().join("received");
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        while std::fs::read_to_string(&received).ok().as_deref() != Some("8") {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("eight calls reach the Tick");
    let ninth = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        session
            .client()
            .call("hold", json!({}), "1", CancellationToken::new()),
    )
    .await
    .expect("a ninth call is refused at once");
    assert!(matches!(ninth, Err(SessionError::Busy)), "{ninth:?}");
    std::fs::write(directory.path().join("release"), "").unwrap();
    let mut answered = 0;
    while let Some(answer) = held.join_next().await {
        let payload = answer.unwrap().unwrap()["payload"].as_str().unwrap().len();
        assert!(payload > tick_protocol::MAX_TOOL_RESPONSE_BYTES - 8192);
        answered += payload;
    }
    assert!(answered > 8 * (tick_protocol::MAX_TOOL_RESPONSE_BYTES - 8192));
    let calls = session
        .client()
        .call("calls", json!({}), "1", CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(calls["calls"], 8, "the ninth call never reached the Tick");
    session.shutdown().await.unwrap();
}

#[tokio::test]
async fn client_binding_rejects_a_different_host_or_terminal() {
    let (session, directory) = counter().await;
    let wrong_host = SessionClient::connect(
        session.instance().clone(),
        SessionBackend::Herdr {
            socket: PathBuf::from("/tmp/wrong-herdr.sock"),
            terminal_id: COUNTER_PANE.into(),
        },
        Some(directory.path().join("sessions")),
    )
    .await;
    assert!(matches!(wrong_host, Err(SessionError::Protocol(_))));
    let wrong_terminal = SessionClient::connect(
        session.instance().clone(),
        herdr_backend("other-pane"),
        Some(directory.path().join("sessions")),
    )
    .await;
    assert!(matches!(wrong_terminal, Err(SessionError::Protocol(_))));
    assert_eq!(
        session.client().read().await.unwrap().data,
        json!({"value":0})
    );
    session.shutdown().await.unwrap();
}

#[tokio::test]
async fn oversized_child_response_is_never_reported_as_success() {
    let directory = tempfile::tempdir().unwrap();
    let session = spawn_tick(
        directory.path(),
        "oversized",
        r#"
import json, sys
for line in sys.stdin:
    request = json.loads(line)
    if request['method'] == 'tick/describe':
        result = {'protocol':3,'revision':'1','kind':'example.large','title':'Large','description':'Fixture','recover_with':'current','tools':[{'name':'large','description':'Large','input_schema':{'type':'object'},'mutates':True},{'name':'current','description':'Read','input_schema':{'type':'object'},'mutates':False}]}
    elif request['method'] == 'tick/read':
        result = {'seq':0,'at':'2026-09-24T00:00:00Z','data':{'value':0}}
    else:
        result = {'text':'x' * (8 * 1024 * 1024)}
    print(json.dumps({'jsonrpc':'2.0','id':request['id'],'result':result}), flush=True)
"#,
    )
    .await;
    let result = session
        .client()
        .call("large", json!({}), "1", CancellationToken::new())
        .await;
    assert!(matches!(result, Err(SessionError::OutcomeUnknown)));
    let _ = session.shutdown().await;
}

#[tokio::test]
async fn child_that_stops_reading_stdin_expires_and_exits_instead_of_hanging_session() {
    let directory = tempfile::tempdir().unwrap();
    let session = spawn_tick(
        directory.path(),
        "nonreader",
        r#"
import json, sys, time
for line in sys.stdin:
    request = json.loads(line)
    if request['method'] == 'tick/describe':
        result = {'protocol':3,'revision':'1','kind':'example.nonreader','title':'Nonreader','description':'Fixture','recover_with':None,'tools':[]}
    elif request['method'] == 'tick/read':
        result = {'seq':0,'at':'2026-09-24T00:00:00Z','data':{}}
    else:
        raise RuntimeError('unexpected request')
    print(json.dumps({'jsonrpc':'2.0','id':request['id'],'result':result}), flush=True)
    if request['method'] == 'tick/read':
        time.sleep(60)
"#,
    )
    .await;
    let (_, _, mut observations) = session.client().watch().await.unwrap();
    let mut stream = tokio::net::UnixStream::connect(session.endpoint())
        .await
        .unwrap();
    let request = json!({
        "jsonrpc":"2.0", "id":1, "method":"tick/describe",
        "binding":{"instance_id":session.instance().to_string(),"backend":{"kind":"herdr","socket":HERDR_SOCKET,"terminal_id":"nonreader-pane"}},
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
