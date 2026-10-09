//! A Tick's responses are parsed and forwarded re-encoded, so their limits apply to the
//! encoding: a baseline at the read limit still reaches a watcher with its frame's envelope, and
//! a dispatched call whose answer only grows past the limit once encoded has an unknown outcome
//! while the Tick keeps serving.

use std::path::PathBuf;

use serde_json::json;
use tick_connect::protocol::MAX_TOOL_RESPONSE_BYTES;
use tick_connect::session::{
    SessionBackend, SessionClient, SessionError, SessionLaunch, TickSession,
};
use tokio_util::sync::CancellationToken;

/// Values the `grow` tool answers with: written as `1e15` its line stays within the response
/// limit, while each re-encodes as `1000000000000000.0`, far over it.
const GROWN_VALUES: usize = 900_000;

/// Room on the Tick's own line for the JSON-RPC envelope around its read.
const TICK_LINE_ENVELOPE: usize = 48;

/// A Tick whose baseline encodes to exactly `argv[1]` bytes and whose `grow` tool answers with
/// numbers that grow when re-encoded.
const LIMITS_TICK: &str = r#"
import json, os, sys
instance = os.environ['TICKLOOP_INSTANCE_ID']
read_bytes = int(sys.argv[1])
def encoded(value):
    return json.dumps(value, separators=(',', ':'))
read = {'version':3,'tick_instance_id':instance,'initialized':True,'sequence':1,'observed_at':'2026-10-08T00:00:00Z','data':{'text':''}}
read['data']['text'] = 'a' * (read_bytes - len(encoded(read)))
for line in sys.stdin:
    request = json.loads(line)
    method = request['method']
    if method == 'tick/describe':
        result = {'protocol':2,'tick_type':'test.limits','title':'Limits','description':'Answers at its limits','recover_with':None,'tools':[{'name':'grow','description':'Change state and answer with growing numbers','params':{'type':'object'},'mutates':True},{'name':'echo','description':'Answer briefly','params':{'type':'object'},'mutates':False}]}
    elif method == 'tick/read':
        result = read
    elif request['params']['tool'] == 'grow':
        sys.stdout.write('{"jsonrpc":"2.0","id":%d,"result":{"values":[%s]}}\n' % (request['id'], ','.join(['1e15'] * int(sys.argv[2]))))
        sys.stdout.flush()
        continue
    else:
        result = {'echo':True}
    sys.stdout.write(encoded({'jsonrpc':'2.0','id':request['id'],'result':result}) + '\n')
    sys.stdout.flush()
"#;

fn backend() -> SessionBackend {
    SessionBackend::Herdr {
        socket: PathBuf::from("/tmp/example-herdr.sock"),
        terminal_id: "limits-pane".into(),
    }
}

async fn limits_tick(read_bytes: usize) -> (TickSession, SessionClient, tempfile::TempDir) {
    let directory = tempfile::tempdir().unwrap();
    let script = directory.path().join("limits.py");
    std::fs::write(&script, LIMITS_TICK).unwrap();
    let session = TickSession::spawn(SessionLaunch {
        command: vec![
            "python3".into(),
            "-u".into(),
            script.to_string_lossy().into_owned(),
            read_bytes.to_string(),
            GROWN_VALUES.to_string(),
        ],
        cwd: directory.path().into(),
        backend: backend(),
        runtime_dir: Some(directory.path().join("sessions")),
    })
    .await
    .unwrap();
    let client = SessionClient::connect(
        session.instance().clone(),
        backend(),
        Some(directory.path().join("sessions")),
    )
    .await
    .unwrap();
    (session, client, directory)
}

#[tokio::test]
async fn a_baseline_at_the_read_limit_reaches_a_watcher_inside_its_frame() {
    let read_bytes = MAX_TOOL_RESPONSE_BYTES - TICK_LINE_ENVELOPE;
    let (_session, client, _directory) = limits_tick(read_bytes).await;
    let (baseline, received, _observations) = client.watch().await.unwrap();
    assert_eq!(serde_json::to_vec(&baseline).unwrap().len(), read_bytes);
    assert_eq!(received, 0);
}

#[tokio::test]
async fn a_call_answered_over_the_limit_once_encoded_has_an_unknown_outcome_and_the_tick_keeps_serving()
 {
    let (_session, client, _directory) = limits_tick(1024).await;
    let answered = client
        .call("grow", json!({}), CancellationToken::new())
        .await
        .unwrap_err();
    assert!(
        matches!(answered, SessionError::OutcomeUnknown),
        "the Tick acted on the call, so its outcome is unknown, not refused: {answered}"
    );
    assert_eq!(
        client
            .call("echo", json!({}), CancellationToken::new())
            .await
            .unwrap(),
        json!({"echo": true})
    );
}
