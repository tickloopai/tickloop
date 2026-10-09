use serde_json::json;
use std::path::{Path, PathBuf};
use tickhost_connect::session::{SessionError, SessionLaunch, SessionLinkState, TickSession};
use tokio_util::sync::CancellationToken;

/// Runs `command` as a Tick Session in `directory`.
async fn spawn(directory: &Path, command: Vec<String>) -> Result<TickSession, SessionError> {
    TickSession::spawn(SessionLaunch {
        command,
        cwd: directory.into(),
        backend: tickhost_connect::session::SessionBackend::Herdr {
            socket: PathBuf::from("/tmp/example-herdr.sock"),
            terminal_id: "fixture-terminal".into(),
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
    let counter = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tick/examples/counter.py");
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
    let (baseline, _, mut observations) = client.watch().await.unwrap();
    assert_eq!(baseline.data, json!({"value":0}));

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
    assert_eq!(update.data, json!({"value":5}));
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
    assert_eq!(read.data, json!({"value":5}), "the stale call never ran");
    assert_eq!(read.seq, 1);
    session.shutdown().await.unwrap();
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

#[tokio::test]
async fn an_unknown_notification_is_counted_without_ending_the_tick() {
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
        print(json.dumps({'jsonrpc':'2.0','method':'tick/progress','params':{'done':0.5}}), flush=True)
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
    assert_eq!(status.ignored_notifications, 1);
    assert_eq!(status.received, 1);
    session.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_hosted_session_answers_only_its_own_terminal_binding() {
    use tickhost_connect::{
        hosted::HostedTerminal,
        session::{SessionBackend, SessionClient},
    };
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
        backend: SessionBackend::Hosted(terminal.clone()),
        runtime_dir: Some(sessions.clone()),
    })
    .await
    .unwrap();
    let instance = session.instance().clone();
    assert!(instance.hosted_run().is_some(), "{instance}");
    SessionClient::connect(
        instance.clone(),
        SessionBackend::Hosted(terminal.clone()),
        Some(sessions.clone()),
    )
    .await
    .expect("the binding it was launched with");
    let restarted_host = HostedTerminal {
        epoch: uuid::Uuid::new_v4(),
        ..terminal.clone()
    };
    assert!(
        SessionClient::connect(
            instance.clone(),
            SessionBackend::Hosted(restarted_host),
            Some(sessions.clone()),
        )
        .await
        .is_err(),
        "another host epoch is another binding"
    );
    assert!(
        SessionClient::connect(
            instance,
            SessionBackend::Herdr {
                socket: PathBuf::from("/tmp/example-herdr.sock"),
                terminal_id: "fixture-terminal".into(),
            },
            Some(sessions),
        )
        .await
        .is_err(),
        "a hosted instance never belongs to a Herdr terminal"
    );
    session.shutdown().await.unwrap();
}
