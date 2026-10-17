use serde_json::json;
use std::path::{Path, PathBuf};
use tickhost_connect::session::{SessionError, SessionLaunch, SessionLinkState, TickSession};
use tokio_util::sync::CancellationToken;

/// Runs `command` as a Tick Session in `directory`.
async fn spawn(directory: &Path, command: Vec<String>) -> Result<TickSession, SessionError> {
    TickSession::spawn(SessionLaunch {
        command,
        cwd: directory.into(),
        terminal: tickhost_connect::hosted::HostedTerminal {
            endpoint: PathBuf::from("/tmp/example-tickhost.sock"),
            epoch: uuid::Uuid::from_u128(1),
            terminal: uuid::Uuid::from_u128(2),
        },
        runtime_dir: Some(directory.join("sessions")),
    })
    .await
}

/// Runs the Python Tick `source` as a Tick Session in `directory`.
async fn spawn_python(directory: &Path, source: &str) -> Result<TickSession, SessionError> {
    let script = directory.join("tick.py");
    std::fs::write(&script, source).unwrap();
    spawn(
        directory,
        vec![
            "python3".into(),
            "-u".into(),
            script.to_string_lossy().into_owned(),
        ],
    )
    .await
}

#[tokio::test]
async fn the_example_counter_is_described_read_called_and_updates_through_its_standard_streams() {
    let directory = tempfile::tempdir().unwrap();
    let counter = PathBuf::from(
        std::env::var_os("CARGO_MANIFEST_DIR").expect("Cargo sets the test package directory"),
    )
    .join("../../tick/examples/counter.py");
    let session = spawn(
        directory.path(),
        vec![
            "python3".into(),
            "-u".into(),
            counter.to_string_lossy().into_owned(),
        ],
    )
    .await
    .unwrap();
    let client = session.client();
    let descriptor = client.describe().await.unwrap();
    assert_eq!(descriptor.protocol, 3);
    assert_eq!(descriptor.kind, "example.counter");
    assert_eq!(descriptor.recover_with.as_deref(), Some("history"));
    assert_eq!(
        descriptor.state.unwrap().series,
        [tick_protocol::StateSeries {
            path: "/recent".into(),
            newest: tick_protocol::SeriesEnd::Last,
        }]
    );
    let (baseline, _, mut observations) = client.watch().await.unwrap();
    assert_eq!(baseline.data, json!({"value":0,"recent":[]}));

    let result = client
        .call(
            "increment",
            json!({"amount":5}),
            &descriptor.revision,
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(result, json!({"value":5}));
    let observation = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            observations.changed().await.unwrap();
            let observation = observations.borrow_and_update().clone();
            if observation.update.is_some() {
                return observation;
            }
        }
    })
    .await
    .expect("the increment published a Tick Update");
    let update = observation.update.unwrap();
    assert_eq!(update.seq, 1);
    assert_eq!(
        update.data,
        json!({"value":5,"recent":[{"seq":1,"value":5}]})
    );
    assert!(
        update.at.is_some(),
        "the host fills in when it received the update"
    );

    let stale = client
        .call(
            "increment",
            json!({"amount":1}),
            "0",
            CancellationToken::new(),
        )
        .await;
    assert!(
        matches!(stale, Err(SessionError::RevisionMismatch)),
        "{stale:?}"
    );
    let read = client.read().await.unwrap();
    assert_eq!(
        read.data,
        json!({"value":5,"recent":[{"seq":1,"value":5}]}),
        "the stale call never ran"
    );
    assert_eq!(read.seq, 1);
    session.shutdown().await.unwrap();
}

/// The example counter drops a line naming no request it could answer, prints nothing for it,
/// and keeps serving the requests after it.
#[test]
fn the_example_counter_drops_lines_it_cannot_answer() {
    let counter = PathBuf::from(
        std::env::var_os("CARGO_MANIFEST_DIR").expect("Cargo sets the test package directory"),
    )
    .join("../../tick/examples/counter.py");
    let mut child = std::process::Command::new("python3")
        .arg("-u")
        .arg(counter)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let padding = "x".repeat(tick_protocol::MAX_REQUEST_BYTES);
    let lines = [
        "not json".to_owned(),
        json!({"jsonrpc":"2.0","method":"tick/read"}).to_string(),
        json!({"jsonrpc":"2.0","id":null,"method":"tick/read"}).to_string(),
        json!({"jsonrpc":"2.0","id":1,"method":"tick/read","params":{"padding":padding}})
            .to_string(),
        json!({"jsonrpc":"2.0","id":2,"method":"tick/read"}).to_string(),
    ];
    std::io::Write::write_all(
        &mut child.stdin.take().unwrap(),
        format!("{}\n", lines.join("\n")).as_bytes(),
    )
    .unwrap();
    let output = child.wait_with_output().unwrap();
    let answers: Vec<serde_json::Value> = String::from_utf8(output.stdout)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(
        answers,
        [json!({"jsonrpc":"2.0","id":2,"result":{"seq":0,"data":{"value":0,"recent":[]}}})]
    );
    assert!(
        output.stderr.is_empty(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// A Tick may not answer under a null `id`, so one that does breaks the session.
#[tokio::test]
async fn a_response_with_a_null_id_breaks_the_session() {
    let directory = tempfile::tempdir().unwrap();
    let session = spawn_python(
        directory.path(),
        r#"
import json, sys
for line in sys.stdin:
    request = json.loads(line)
    if request['method'] == 'tick/describe':
        result = {'protocol':3,'revision':'1','kind':'example.nullid','title':'Null id','description':'Fixture','recover_with':'current','tools':[{'name':'increment','description':'Increment','input_schema':{'type':'object'},'mutates':True},{'name':'current','description':'Read','input_schema':{'type':'object'},'mutates':False}]}
    elif request['method'] == 'tick/read':
        result = {'seq':0,'at':'2026-09-24T00:00:00Z','data':{'value':0}}
    else:
        print(json.dumps({'jsonrpc':'2.0','id':None,'error':{'code':-32600,'message':'refused'}}), flush=True)
        continue
    print(json.dumps({'jsonrpc':'2.0','id':request['id'],'result':result}), flush=True)
"#,
    )
    .await
    .unwrap();
    let (_, _, mut observations) = session.client().watch().await.unwrap();
    let result = session
        .client()
        .call("increment", json!({}), "1", CancellationToken::new())
        .await;
    assert!(
        matches!(result, Err(SessionError::OutcomeUnknown)),
        "{result:?}"
    );
    let state = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        observations.wait_for(|observation| observation.state != SessionLinkState::Live),
    )
    .await
    .unwrap()
    .unwrap()
    .state
    .clone();
    assert!(
        matches!(state, SessionLinkState::ProtocolError(_)),
        "{state:?}"
    );
    let _ = session.shutdown().await;
}

/// Adds a tool, and so a new catalog revision, on `evolve`; `count` reads how often
/// `increment` ran.
const EVOLVING: &str = r#"
import json, sys
revision, value = 1, 0
def descriptor():
    tools = [{'name':'increment','description':'Increment','input_schema':{'type':'object'},'mutates':True},{'name':'evolve','description':'Add a tool','input_schema':{'type':'object'},'mutates':True}]
    if revision > 1:
        tools.append({'name':'reset','description':'Reset','input_schema':{'type':'object'},'mutates':True})
    return {'protocol':3,'kind':'example.evolving','title':'Evolving','description':'Changes its catalog','revision':str(revision),'tools':tools}
for line in sys.stdin:
    request = json.loads(line)
    method, params = request['method'], request.get('params', {})
    if method == 'tick/describe':
        reply = {'result': descriptor()}
    elif method == 'tick/read':
        reply = {'result': {'seq':0,'data':{'value':value}}}
    elif params.get('revision') != str(revision):
        reply = {'error': {'code':-32004,'message':'catalog changed'}}
    elif params['tool'] == 'evolve':
        revision += 1
        reply = {'result': {'revision':str(revision)}}
    else:
        value += 1
        reply = {'result': {'value':value}}
    print(json.dumps({'jsonrpc':'2.0','id':request['id'], **reply}), flush=True)
"#;

#[tokio::test]
async fn a_call_prepared_against_an_older_catalog_revision_is_refused_unrun() {
    let directory = tempfile::tempdir().unwrap();
    let session = spawn_python(directory.path(), EVOLVING).await.unwrap();
    let client = session.client();
    let prepared = client.describe().await.unwrap();
    client
        .call(
            "evolve",
            json!({}),
            &prepared.revision,
            CancellationToken::new(),
        )
        .await
        .unwrap();
    let current = client.describe().await.unwrap();
    assert_ne!(current.revision, prepared.revision);

    let stale = client
        .call(
            "increment",
            json!({}),
            &prepared.revision,
            CancellationToken::new(),
        )
        .await;
    assert!(
        matches!(stale, Err(SessionError::RevisionMismatch)),
        "{stale:?}"
    );
    assert_eq!(client.read().await.unwrap().data, json!({"value":0}));
    assert_eq!(
        client
            .call(
                "increment",
                json!({}),
                &current.revision,
                CancellationToken::new()
            )
            .await
            .unwrap(),
        json!({"value":1})
    );
    session.shutdown().await.unwrap();
}

/// Answers each call with the JSON-RPC error its tool names, or with nothing at all for
/// `garbled`.
const ANSWERS_WITH_ERRORS: &str = r#"
import json, sys
errors = {'no_calls':-32601,'bad_args':-32602,'jam':-32000,'own_code':-32050}
for line in sys.stdin:
    request = json.loads(line)
    reply = {}
    if request['method'] == 'tick/describe':
        reply['result'] = {'protocol':3,'kind':'example.failing','title':'Failing','description':'Fails','revision':'1','tools':[]}
    elif request['method'] == 'tick/read':
        reply['result'] = {'seq':0,'data':None}
    elif request['params']['tool'] != 'garbled':
        reply['error'] = {'code':errors[request['params']['tool']],'message':'refused by ' + request['params']['tool']}
    print(json.dumps({'jsonrpc':'2.0','id':request['id'], **reply}), flush=True)
"#;

#[tokio::test]
async fn a_tick_error_says_whether_the_call_ran_and_only_a_garbled_answer_leaves_it_unknown() {
    let directory = tempfile::tempdir().unwrap();
    let session = spawn_python(directory.path(), ANSWERS_WITH_ERRORS)
        .await
        .unwrap();
    let client = session.client();
    let call = |tool: &'static str| {
        let client = client.clone();
        async move {
            client
                .call(tool, json!({}), "1", CancellationToken::new())
                .await
        }
    };
    for refusal in ["no_calls", "bad_args"] {
        match call(refusal).await {
            Err(SessionError::Refused(message)) => {
                assert!(message.contains(refusal), "{message}")
            }
            other => panic!("{refusal} was refused unrun: {other:?}"),
        }
    }
    for failure in ["jam", "own_code"] {
        match call(failure).await {
            Err(SessionError::ToolFailed(message)) => {
                assert!(message.contains(failure), "{message}")
            }
            other => panic!("{failure} ran and failed: {other:?}"),
        }
    }
    let garbled = call("garbled").await;
    assert!(
        matches!(garbled, Err(SessionError::OutcomeUnknown)),
        "an answer with neither result nor error after dispatch: {garbled:?}"
    );
    session.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_protocol_2_tick_fails_to_start_with_a_version_error() {
    let directory = tempfile::tempdir().unwrap();
    let refused = spawn_python(
        directory.path(),
        r#"
import json, sys
for line in sys.stdin:
    request = json.loads(line)
    result = {'protocol':2,'tick_type':'example.old','title':'Old','description':'Protocol 2','recover_with':None,'tools':[]}
    print(json.dumps({'jsonrpc':'2.0','id':request['id'],'result':result}), flush=True)
"#,
    )
    .await;
    match refused {
        Err(SessionError::Protocol(message)) => assert_eq!(
            message,
            "unsupported Tick protocol version 2; this host speaks Tick protocol 3"
        ),
        other => panic!("a protocol 2 Tick must not start: {:?}", other.map(|_| ())),
    }
}

/// Notifications this host does not know are counted, and the first of each method is logged
/// next to the session's socket rather than on the Tick's terminal.
#[tokio::test]
async fn an_unknown_notification_is_counted_and_logged_without_ending_the_tick() {
    let directory = tempfile::tempdir().unwrap();
    let session = spawn_python(
        directory.path(),
        r#"
import json, sys
for line in sys.stdin:
    request = json.loads(line)
    if request['method'] == 'tick/describe':
        result = {'protocol':3,'kind':'example.chatty','title':'Chatty','description':'Sends a later notification','revision':'1','tools':[{'name':'poke','description':'Poke','input_schema':{'type':'object'},'mutates':True}]}
    elif request['method'] == 'tick/read':
        result = {'seq':0,'data':None}
    else:
        for method in ['tick/progress', 'tick/progress', 'tick/later']:
            print(json.dumps({'jsonrpc':'2.0','method':method,'params':{'done':0.5}}), flush=True)
        print(json.dumps({'jsonrpc':'2.0','method':'tick/update','params':{'seq':1,'data':{'poked':True}}}), flush=True)
        result = {'poked':True}
    print(json.dumps({'jsonrpc':'2.0','id':request['id'],'result':result}), flush=True)
"#,
    )
    .await
    .unwrap();
    let client = session.client();
    let (baseline, _, mut observations) = client.watch().await.unwrap();
    assert!(!baseline.is_ready(), "null data means no content yet");
    client
        .call("poke", json!({}), "1", CancellationToken::new())
        .await
        .unwrap();
    let observation = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            observations.changed().await.unwrap();
            let observation = observations.borrow_and_update().clone();
            if observation.update.is_some() {
                return observation;
            }
        }
    })
    .await
    .expect("the update after the unknown notification arrived");
    assert_eq!(observation.state, SessionLinkState::Live);
    assert_eq!(observation.update.unwrap().data, json!({"poked":true}));
    let status = client.status().await.unwrap();
    assert_eq!(status.ignored_notifications, 3);
    assert_eq!(status.received, 1);
    let log_path = session.endpoint().with_extension("log");
    let log = std::fs::read_to_string(&log_path).unwrap();
    let lines: Vec<_> = log.lines().collect();
    assert_eq!(lines.len(), 2, "one line for each unknown method: {log}");
    assert!(lines[0].contains("\"tick/progress\""), "{log}");
    assert!(lines[1].contains("\"tick/later\""), "{log}");
    assert!(lines[0].contains(&session.instance().to_string()), "{log}");
    session.shutdown().await.unwrap();
    assert!(!log_path.exists(), "the log goes with its session");
}

/// Answers its first read with seq 0 and writes the update to seq 1 right behind it, in the same
/// write, so the session reads both frames before the reader of the answer runs.
const UPDATES_RIGHT_AFTER_ITS_FIRST_READ: &str = r#"
import json, sys
seq = 0
for line in sys.stdin:
    request = json.loads(line)
    if request['method'] == 'tick/describe':
        result = {'protocol':3,'kind':'example.racing','title':'Racing','description':'Updates right after a read','revision':'1','tools':[]}
    else:
        result = {'seq':seq,'data':{'value':seq}}
    frames = [{'jsonrpc':'2.0','id':request['id'],'result':result}]
    if request['method'] == 'tick/read' and seq == 0:
        seq = 1
        frames.append({'jsonrpc':'2.0','method':'tick/update','params':{'seq':seq,'data':{'value':seq}}})
    sys.stdout.write(''.join(json.dumps(frame) + '\n' for frame in frames))
    sys.stdout.flush()
"#;

#[tokio::test]
async fn a_read_answer_is_checked_before_the_update_that_arrived_after_it() {
    let directory = tempfile::tempdir().unwrap();
    let session = spawn_python(directory.path(), UPDATES_RIGHT_AFTER_ITS_FIRST_READ)
        .await
        .expect("read(0) on the wire before update(1) is in order");
    let client = session.client();
    let (baseline, received, observations) = client.watch().await.unwrap();
    assert_eq!(baseline.seq, 1, "the later update is the newest state");
    assert_eq!(received, 1);
    assert_eq!(observations.borrow().state, SessionLinkState::Live);
    assert_eq!(client.read().await.unwrap().seq, 1);
    session.shutdown().await.unwrap();
}

/// Has already moved to seq 1 when it starts, but writes that update only when `flush` is
/// called: its reads answer seq 1 before the update with the same seq is on the wire.
const ANSWERS_A_READ_BEFORE_WRITING_ITS_UPDATE: &str = r#"
import json, sys
state = {'seq':1,'data':{'value':1}}
for line in sys.stdin:
    request = json.loads(line)
    if request['method'] == 'tick/describe':
        result = {'protocol':3,'kind':'example.lagging','title':'Lagging','description':'Writes its update late','revision':'1','tools':[{'name':'flush','description':'Write the pending update','input_schema':{'type':'object'},'mutates':False}]}
    elif request['method'] == 'tick/read':
        result = state
    else:
        print(json.dumps({'jsonrpc':'2.0','method':'tick/update','params':state}), flush=True)
        result = {}
    print(json.dumps({'jsonrpc':'2.0','id':request['id'],'result':result}), flush=True)
"#;

#[tokio::test]
async fn an_update_may_follow_a_read_that_already_answered_its_seq() {
    let directory = tempfile::tempdir().unwrap();
    let session = spawn_python(directory.path(), ANSWERS_A_READ_BEFORE_WRITING_ITS_UPDATE)
        .await
        .unwrap();
    let client = session.client();
    let (baseline, _, mut observations) = client.watch().await.unwrap();
    assert_eq!(baseline.seq, 1);
    client
        .call("flush", json!({}), "1", CancellationToken::new())
        .await
        .expect("update(1) after read(1) is the Tick's first update, so it is in order");
    let observation = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            observations.changed().await.unwrap();
            let observation = observations.borrow_and_update().clone();
            if observation.update.is_some() {
                return observation;
            }
        }
    })
    .await
    .expect("the update arrived");
    assert_eq!(observation.state, SessionLinkState::Live);
    assert_eq!(observation.update.unwrap().seq, 1);
    session.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_hosted_session_answers_only_its_own_terminal_binding() {
    use tickhost_connect::{hosted::HostedTerminal, session::SessionClient};
    let directory = tempfile::tempdir().unwrap();
    let script = directory.path().join("quiet.py");
    std::fs::write(&script, r#"
import json, sys
for line in sys.stdin:
    request = json.loads(line)
    if request['method'] == 'tick/describe':
        result = {'protocol':3,'revision':'1','kind':'example.quiet','title':'Quiet','description':'Quiet','recover_with':None,'tools':[]}
    else:
        result = {'seq':0,'at':'2026-10-07T00:00:00Z','data':{}}
    print(json.dumps({'jsonrpc':'2.0','id':request['id'],'result':result}),flush=True)
"#).unwrap();
    let terminal = HostedTerminal {
        endpoint: directory.path().join("host.sock"),
        epoch: uuid::Uuid::new_v4(),
        terminal: uuid::Uuid::new_v4(),
    };
    let sessions = directory.path().join("sessions");
    let session = TickSession::spawn(SessionLaunch {
        command: vec![
            "python3".into(),
            "-u".into(),
            script.to_string_lossy().into_owned(),
        ],
        cwd: directory.path().into(),
        terminal: terminal.clone(),
        runtime_dir: Some(sessions.clone()),
    })
    .await
    .unwrap();
    let instance = session.instance().clone();
    assert!(instance.hosted_run().is_some(), "{instance}");
    SessionClient::connect(instance.clone(), terminal.clone(), Some(sessions.clone()))
        .await
        .expect("the binding it was launched with");
    let restarted_host = HostedTerminal {
        epoch: uuid::Uuid::new_v4(),
        ..terminal.clone()
    };
    assert!(
        SessionClient::connect(instance.clone(), restarted_host, Some(sessions.clone()),)
            .await
            .is_err(),
        "another host epoch is another binding"
    );
    let other_terminal = HostedTerminal {
        terminal: uuid::Uuid::new_v4(),
        ..terminal
    };
    assert!(
        SessionClient::connect(instance, other_terminal, Some(sessions))
            .await
            .is_err(),
        "another terminal of the same host is another binding"
    );
    session.shutdown().await.unwrap();
}
