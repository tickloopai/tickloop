use super::*;
use tickloop_harness::protocol::{Content, ExecutionMode, OperationState};
use tickloop_harness::runtime::environment::{
    ExecutionAuthority, ExecutionEnvironment, ExecutionIntent, ExecutionOutcome,
};
use tickloop_harness::test_support::{ScriptedModel, auto_classification, call, next_event, text};
use tokio::io::AsyncWriteExt;
use tokio::net::{UnixListener, UnixStream};

// Session startup and Tickhost registration each describe the Tick. The third, title describe waits
// for the test to release a socket, after announcing that it reached the cancellation window.
const TITLE_GATE_TICK: &str = r#"
import json, os, socket, sys
describes = 0
with open('starts', 'a') as log:
    log.write(str(os.getpid()) + '\n')
for line in sys.stdin:
    request = json.loads(line)
    if request['method'] == 'tick/describe':
        describes += 1
        if describes == 3 and os.path.exists('hold-title'):
            with socket.socket(socket.AF_UNIX) as gate:
                gate.connect('title.sock')
                gate.recv(1)
        result = {'protocol':3,'revision':'1','kind':'test.title','title':'Declared title','description':'Title gate','tools':[]}
    else:
        result = {'seq':0,'data':{}}
    print(json.dumps({'jsonrpc':'2.0','id':request['id'],'result':result}), flush=True)
"#;

#[derive(Clone, Copy, Debug)]
enum OpenCase {
    Hidden,
    Reopen,
}

struct TitleGate {
    listener: UnixListener,
    input: Value,
    lost: Option<tick_protocol::InstanceRef>,
}

impl TitleGate {
    async fn prepare(daemon: &Daemon, case: OpenCase) -> Self {
        let script = daemon.root.join("title-gate.py");
        std::fs::write(&script, TITLE_GATE_TICK).unwrap();
        let mut input = open_command(daemon, python_program(&script, &[]));
        let lost = match case {
            OpenCase::Reopen => {
                let opened = run_tool(&daemon.tools(), daemon.chats[0], "TickManage", input).await;
                let instance = serde_json::from_value(opened["tick_instance_id"].clone()).unwrap();
                let pid = opened["business_pid"].as_u64().unwrap() as u32;
                let launcher = tickhost::process::sample(pid).unwrap().parent;
                let host = tickhost::process::sample(launcher).unwrap().parent;
                unsafe { libc::kill(host as libc::pid_t, libc::SIGKILL) };
                assert!(super::super::gone(pid).await);
                input = json!({"action":"reopen","tick_instance_id":instance});
                Some(instance)
            }
            OpenCase::Hidden => None,
        };
        std::fs::write(daemon.root.join("hold-title"), "").unwrap();
        Self {
            listener: UnixListener::bind(daemon.root.join("title.sock")).unwrap(),
            input,
            lost,
        }
    }

    async fn title_requested(&self) -> UnixStream {
        tokio::time::timeout(super::super::FIXTURE_TIMEOUT, self.listener.accept())
            .await
            .expect("the post-open title describe must reach the gate")
            .unwrap()
            .0
    }

    async fn assert_single_open(&self, daemon: &Daemon) -> Value {
        let listed = DaemonControl::new(daemon.grants[0].clone())
            .request(list(daemon.chats[0]))
            .await
            .unwrap();
        let rows = listed["items"].as_array().unwrap();
        assert_eq!(rows.len(), 1, "{listed}");
        assert_eq!(
            std::fs::read_to_string(daemon.root.join("starts"))
                .unwrap()
                .lines()
                .count(),
            if self.lost.is_some() { 2 } else { 1 },
            "each explicit open or reopen starts exactly one Tick"
        );
        rows[0]["instance"].clone()
    }

    fn assert_result(&self, result: &ToolResult, instance: &Value, case: OpenCase) {
        assert!(!result.is_error, "{}", result.content);
        let opened: Value = serde_json::from_str(&result.content).unwrap();
        assert_eq!(opened["status"], "opened", "{opened}");
        let returned_instance: tick_protocol::InstanceRef =
            serde_json::from_value(opened["tick_instance_id"].clone()).unwrap();
        assert_eq!(json!(returned_instance), *instance);
        assert!(opened.get("visibility").is_none(), "{opened}");
        assert!(opened["business_pid"].is_u64());
        if let Some(lost) = &self.lost {
            assert_eq!(
                serde_json::from_value::<tick_protocol::InstanceRef>(opened["replaces"].clone())
                    .unwrap(),
                *lost
            );
            assert_ne!(*instance, json!(lost));
        }
        assert!(opened.get("view").is_none(), "{case:?}: {opened}");
    }
}

async fn confirmed_open_survives_interruption(case: OpenCase, revoke: bool) {
    let directory = tempfile::tempdir_in(tickloop_harness::test_support::SHORT_TEMP_ROOT).unwrap();
    let herdr = RecordingHerdr::start(directory.path().join("herdr.sock"));
    let daemon = Daemon::start_in_herdr(
        Arc::new(ScriptedModel::new(Vec::new())),
        herdr.socket.clone(),
        1,
    )
    .await;
    let gate = TitleGate::prepare(&daemon, case).await;
    let authority = ExecutionAuthority::new(daemon.chats[0].into());
    let port = daemon.environments[0]
        .bind(port_scope(daemon.chats[0], &daemon.root), authority.clone())
        .unwrap();
    let intent = if let Some(instance) = &gate.lost {
        ExecutionIntent::Reopen {
            instance: instance.clone(),
        }
    } else {
        ExecutionIntent::Open(port_open(
            serde_json::from_value(gate.input["command"].clone()).unwrap(),
            &daemon.root,
        ))
    };
    let prepared = port.prepare(intent).await.unwrap();
    let admission = authority.admit(&prepared).unwrap();
    let cancel = CancellationToken::new();
    let execution = tokio::spawn({
        let cancel = cancel.clone();
        async move { port.execute(prepared, admission, cancel).await }
    });
    let mut blocked_title = gate.title_requested().await;
    let instance = gate.assert_single_open(&daemon).await;
    if revoke {
        authority.revoke();
    } else {
        cancel.cancel();
    }
    let outcome = tokio::time::timeout(Duration::from_secs(5), execution)
        .await
        .unwrap()
        .unwrap();
    blocked_title.write_all(b"x").await.unwrap();
    assert_eq!(gate.assert_single_open(&daemon).await, instance);
    let ExecutionOutcome::Completed(completed) = outcome else {
        panic!("a confirmed {case:?} must stay completed: {outcome:?}");
    };
    gate.assert_result(&completed.result, &instance, case);
    let effect = completed
        .opened
        .expect("the confirmed open retains its effect");
    assert_eq!(json!(effect.instance), instance);
    assert_eq!(effect.title.as_deref(), Some("python3 -u title-gate.py"));
    assert!(!effect.is_terminal);
    assert!(herdr.methods().is_empty(), "{:?}", herdr.methods());
}

#[tokio::test(flavor = "multi_thread")]
async fn hidden_open_keeps_its_confirmed_effect_when_title_is_cancelled_or_revoked() {
    for revoke in [false, true] {
        confirmed_open_survives_interruption(OpenCase::Hidden, revoke).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn reopen_keeps_its_confirmed_effect_when_title_is_cancelled_or_revoked() {
    for revoke in [false, true] {
        confirmed_open_survives_interruption(OpenCase::Reopen, revoke).await;
    }
}

async fn interrupted_open_records_membership_and_creation(case: OpenCase) {
    let directory = tempfile::tempdir_in(tickloop_harness::test_support::SHORT_TEMP_ROOT).unwrap();
    let herdr = RecordingHerdr::start(directory.path().join("herdr.sock"));
    let model = Arc::new(ScriptedModel::new(Vec::new()));
    let daemon = Daemon::start_in_herdr(model.clone(), herdr.socket.clone(), 1).await;
    let (runtime, chat) = (&daemon.runtimes[0], daemon.chats[0]);
    runtime
        .set_execution_mode(chat.into(), ExecutionMode::FullAccess)
        .await
        .unwrap();
    let gate = TitleGate::prepare(&daemon, case).await;
    model.enqueue(call("open", "TickManage", gate.input.clone()));
    let mut events = runtime.subscribe();
    let receipt = runtime
        .submit(
            chat.into(),
            "Open the Tick and close it after checking it".into(),
        )
        .await
        .unwrap();
    let mut blocked_title = gate.title_requested().await;
    let instance = gate.assert_single_open(&daemon).await;
    runtime.interrupt(chat.into()).await.unwrap();
    let mut result = None;
    loop {
        let event = next_event(&mut events).await;
        match event.kind {
            EventKind::ToolCompleted {
                call_id,
                result: completed,
                ..
            } if call_id == "open" => result = Some(completed),
            EventKind::Interrupted if event.run_id == receipt.run_id => break,
            EventKind::Failed(error) => panic!("{error}"),
            _ => {}
        }
    }
    blocked_title.write_all(b"x").await.unwrap();
    assert_eq!(gate.assert_single_open(&daemon).await, instance);
    gate.assert_result(
        &result.expect("the interrupted open still completes"),
        &instance,
        case,
    );
    let snapshot = runtime.snapshot(chat.into()).await.unwrap();
    let member = snapshot
        .monitor
        .members
        .iter()
        .find(|member| json!(member.instance) == instance)
        .expect("the opened Tick joins Monitor even when its title is cancelled");
    assert_eq!(member.title.as_deref(), Some("python3 -u title-gate.py"));
    assert!(!member.is_terminal);
    let operation = snapshot
        .operations
        .iter()
        .find(|operation| operation.call_id == "open")
        .unwrap();
    assert_eq!(operation.state, OperationState::Completed);
    assert!(
        runtime
            .tick_subscriptions(chat.into())
            .await
            .unwrap()
            .is_empty()
    );
    assert!(herdr.methods().is_empty(), "{:?}", herdr.methods());

    // The Auto review input is the public consumer of the creation receipt, including its
    // binding to this Loop and the completed operation; Monitor membership alone cannot prove it.
    runtime
        .set_execution_mode(chat.into(), ExecutionMode::Auto)
        .await
        .unwrap();
    let reviewer = Arc::new(ScriptedModel::new(vec![text(
        &auto_classification(false).to_string(),
    )]));
    runtime.set_auto_reviewer(reviewer.clone()).await;
    model.enqueue(call(
        "close",
        "TickManage",
        json!({"action":"close","tick_instance_id":instance}),
    ));
    model.enqueue(text("closed"));
    let results = run_turn(runtime, chat, "Close the Tick created for this task").await;
    assert!(!results["close"].is_error, "{}", results["close"].content);
    {
        let requests = reviewer.requests.lock().unwrap();
        let Content::Text { text } = &requests[0].messages[1].content[0] else {
            panic!()
        };
        let context: Value = serde_json::from_str(text).unwrap();
        let created_instance: tick_protocol::InstanceRef =
            serde_json::from_value(context["creation_receipt"]["instance"].clone()).unwrap();
        assert_eq!(json!(created_instance), instance);
        assert_eq!(
            tickloop_ids::base62::decode_either(
                context["creation_receipt"]["operation_id"]
                    .as_str()
                    .unwrap()
            ),
            Some(operation.id)
        );
        assert_eq!(
            tickloop_ids::base62::decode_either(
                context["creation_receipt"]["loop_id"].as_str().unwrap()
            ),
            Some(operation.loop_id.into())
        );
    }
    runtime.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn cancelled_hidden_title_preserves_monitor_membership_and_creation_receipt() {
    interrupted_open_records_membership_and_creation(OpenCase::Hidden).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn cancelled_reopen_title_preserves_monitor_membership_and_creation_receipt() {
    interrupted_open_records_membership_and_creation(OpenCase::Reopen).await;
}
