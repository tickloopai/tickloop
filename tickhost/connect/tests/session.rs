use serde_json::json;
use std::path::PathBuf;
use tick_protocol::descriptor_digest;
use tickhost_connect::session::{SessionLaunch, TickSession};
use tokio_util::sync::CancellationToken;

#[tokio::test]
async fn external_nonfinancial_program_is_read_and_changed_through_its_standard_streams() {
    let directory = tempfile::tempdir().unwrap();
    let script = directory.path().join("counter.py");
    std::fs::write(&script, r#"
import json, os, sys
instance = os.environ['TICKLOOP_INSTANCE_ID']
value = 0
for line in sys.stdin:
    request = json.loads(line)
    method = request['method']
    if method == 'tick/describe':
        result = {'protocol':2,'tick_type':'example.counter','title':'Counter','description':'External counter','recover_with':'current','tools':[{'name':'increment','description':'Increment','params':{'type':'object'},'mutates':True},{'name':'current','description':'Read value','params':{'type':'object'},'mutates':False}]}
    elif method == 'tick/read':
        result = {'version':3,'tick_instance_id':instance,'initialized':True,'sequence':value,'observed_at':'2026-09-24T00:00:00Z','data':{'value':value}}
    elif method == 'tool/call':
        if request['params']['tool'] == 'increment':
            value += 1
            print(json.dumps({'jsonrpc':'2.0','method':'tick/update','params':{'version':3,'tick_instance_id':instance,'source':'example.counter','sequence':value,'observed_at':'2026-09-24T00:00:01Z','data':{'value':value}}}),flush=True)
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
        backend: tickhost_connect::session::SessionBackend::Herdr {
            socket: PathBuf::from("/tmp/example-herdr.sock"),
            terminal_id: "fixture-terminal".into(),
        },
        runtime_dir: Some(directory.path().join("sessions")),
    })
    .await
    .unwrap();
    let client = session.client();
    assert_eq!(client.read().await.unwrap().data, Some(json!({"value":0})));
    let result = client
        .call("increment", json!({}), CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(result, json!({"value":1}));
    assert_eq!(client.read().await.unwrap().data, Some(json!({"value":1})));
    session.shutdown().await.unwrap();
}

#[tokio::test]
async fn child_fence_uses_jcs_of_the_original_descriptor_value() {
    let directory = tempfile::tempdir().unwrap();
    let script = directory.path().join("jcs_counter.py");
    std::fs::write(&script, r#"
import hashlib, json, os, sys
instance = os.environ['TICKLOOP_INSTANCE_ID']
value = 0
descriptor = {'protocol':2,'tick_type':'example.jcs','title':'JCS Counter','description':'External counter','tools':[{'name':'increment','description':'Increment','params':{'type':'object','properties':{'amount':{'type':'number','minimum':1e-7}},'required':['amount'],'additionalProperties':False},'mutates':True}]}
canonical = '{"description":"External counter","protocol":2,"tick_type":"example.jcs","title":"JCS Counter","tools":[{"description":"Increment","mutates":true,"name":"increment","params":{"additionalProperties":false,"properties":{"amount":{"minimum":1e-7,"type":"number"}},"required":["amount"],"type":"object"}}]}'
expected = hashlib.sha256(canonical.encode()).hexdigest()
for line in sys.stdin:
    request = json.loads(line)
    method = request['method']
    if method == 'tick/describe':
        result = descriptor
    elif method == 'tick/read':
        result = {'version':3,'tick_instance_id':instance,'initialized':True,'sequence':value,'observed_at':'2026-09-24T00:00:00Z','data':{'value':value}}
    elif method == 'tool/call':
        if request['params'].get('expected_descriptor_digest') != expected:
            print(json.dumps({'jsonrpc':'2.0','id':request['id'],'error':{'code':-32602,'message':'noncanonical descriptor digest'}}), flush=True)
            continue
        value += 1
        result = {'value':value}
    else:
        raise RuntimeError(method)
    print(json.dumps({'jsonrpc':'2.0','id':request['id'],'result':result}), flush=True)
"#).unwrap();
    let session = TickSession::spawn(SessionLaunch {
        command: vec![
            "python3".into(),
            "-u".into(),
            script.to_string_lossy().into_owned(),
        ],
        cwd: directory.path().into(),
        backend: tickhost_connect::session::SessionBackend::Herdr {
            socket: PathBuf::from("/tmp/example-herdr.sock"),
            terminal_id: "jcs-pane".into(),
        },
        runtime_dir: Some(directory.path().join("sessions")),
    })
    .await
    .unwrap();
    let client = session.client();
    let descriptor = client.describe().await.unwrap();
    assert_eq!(descriptor.recover_with, None);
    let result = client
        .call_prepared(
            "increment",
            json!({"amount":1e-7}),
            &descriptor_digest(&descriptor),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(result, json!({"value":1}));
    assert_eq!(client.read().await.unwrap().data, Some(json!({"value":1})));
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
import json, os, sys
instance = os.environ['TICKLOOP_INSTANCE_ID']
for line in sys.stdin:
    request = json.loads(line)
    if request['method'] == 'tick/describe':
        result = {'protocol':2,'tick_type':'example.quiet','title':'Quiet','description':'Quiet','recover_with':None,'tools':[]}
    else:
        result = {'version':3,'tick_instance_id':instance,'initialized':True,'sequence':0,'observed_at':'2026-10-07T00:00:00Z','data':{}}
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
