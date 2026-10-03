mod support;

use std::{
    collections::BTreeMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};

use async_trait::async_trait;
use serde_json::{Value, json};
use support::{ScriptedModel, call, config, next_event, text};
use tick_connect::{
    access::TickAccess,
    herdr::{HerdrContext, HerdrPort, HostError, Pane, process_birth},
    instance::InstanceRef,
    session::{SessionLaunch, TickSession},
};
use tickloop_agent::{
    runtime::{Agent, Event, EventKind, LoopState, TickLinkState},
    tick_stream::{NativeTickStream, TickConnectionState, TickStreamEvent, TickStreamPort},
};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

struct BlockingTerminalHost {
    inner: TerminalHost,
    reads: AtomicUsize,
    blocked: tokio::sync::Notify,
    release: tokio::sync::Notify,
}

#[async_trait]
impl HerdrPort for BlockingTerminalHost {
    async fn request(
        &self,
        method: &str,
        params: Value,
        cancel: CancellationToken,
    ) -> Result<Value, HostError> {
        if method == "pane.read" && self.reads.fetch_add(1, Ordering::SeqCst) == 1 {
            self.blocked.notify_one();
            self.release.notified().await;
            return Err(HostError::before(
                "IdentityUncertain",
                "the old read no longer proves continuity",
            ));
        }
        self.inner.request(method, params, cancel).await
    }
}

struct TerminalHost {
    text: Mutex<BTreeMap<String, String>>,
    gone: Mutex<Option<String>>,
    protocol: Mutex<Option<InstanceRef>>,
    outage: AtomicBool,
}

impl TerminalHost {
    fn new() -> Self {
        Self {
            text: Mutex::new(BTreeMap::from([
                ("one".into(), "initial one".into()),
                ("two".into(), "initial two".into()),
            ])),
            gone: Mutex::new(None),
            protocol: Mutex::new(None),
            outage: AtomicBool::new(false),
        }
    }

    fn set(&self, pane: &str, content: &str) {
        self.text
            .lock()
            .unwrap()
            .insert(pane.into(), content.into());
    }

    fn pane(&self, pane_id: &str) -> Pane {
        Pane {
            pane_id: pane_id.into(),
            terminal_id: pane_id.into(),
            workspace_id: "workspace".into(),
            tab_id: "tab".into(),
            cwd: None,
            foreground_cwd: None,
            title: Some(pane_id.into()),
            tokens: if pane_id == "protocol" {
                self.protocol
                    .lock()
                    .unwrap()
                    .as_ref()
                    .map(|instance| {
                        BTreeMap::from([("tickloop_instance".into(), instance.to_string())])
                    })
                    .unwrap_or_default()
            } else {
                BTreeMap::new()
            },
        }
    }
}

#[async_trait]
impl HerdrPort for TerminalHost {
    async fn request(
        &self,
        method: &str,
        params: Value,
        _: CancellationToken,
    ) -> Result<Value, HostError> {
        if self.outage.load(Ordering::SeqCst) {
            return Err(HostError::before(
                "HostUnavailable",
                "controlled Herdr connection is down",
            ));
        }
        match method {
            "pane.current" => Ok(json!({"pane": self.pane("agent")})),
            "pane.list" => {
                let gone = self.gone.lock().unwrap();
                let mut panes: Vec<_> = ["one", "two"]
                    .into_iter()
                    .filter(|pane| gone.as_deref() != Some(*pane))
                    .map(|pane| self.pane(pane))
                    .collect();
                if self.protocol.lock().unwrap().is_some() {
                    panes.push(self.pane("protocol"));
                }
                Ok(json!({"panes": panes}))
            }
            "pane.process_info" => Ok(
                json!({"foreground_process_group_id":1,"foreground_processes":[{"pid":std::process::id()}]}),
            ),
            "pane.read" => {
                let pane = params["pane_id"].as_str().unwrap();
                let text = self.text.lock().unwrap().get(pane).cloned().unwrap();
                Ok(
                    json!({"source":"recent_unwrapped","format":"text","text":text,"truncated":false}),
                )
            }
            _ => Err(HostError::before("UnknownMethod", method)),
        }
    }
}

fn instance(pane: &str) -> InstanceRef {
    let birth = process_birth(std::process::id()).unwrap();
    InstanceRef::native(format!("terminal:{pane}:1:{birth}")).unwrap()
}

async fn completed(receiver: &mut tokio::sync::broadcast::Receiver<Event>) {
    loop {
        if matches!(next_event(receiver).await.kind, EventKind::Completed { .. }) {
            return;
        }
    }
}

async fn settle() {
    for _ in 0..20 {
        tokio::task::yield_now().await;
    }
}

fn last_user_text(model: &ScriptedModel) -> String {
    let requests = model.requests.lock().unwrap();
    requests
        .last()
        .unwrap()
        .messages
        .iter()
        .rev()
        .find_map(|message| {
            (message.role == tickloop_agent::model::Role::User)
                .then(|| {
                    message.content.iter().find_map(|content| match content {
                        tickloop_agent::model::Content::Text { text } => Some(text.clone()),
                        _ => None,
                    })
                })
                .flatten()
        })
        .unwrap()
}

#[tokio::test(start_paused = true)]
async fn terminal_text_changes_reach_only_the_owning_loops_after_their_baselines() {
    let directory = tempfile::tempdir().unwrap();
    let one = instance("one");
    let two = instance("two");
    let model = Arc::new(ScriptedModel::new(vec![
        call(
            "s1",
            "SubscribeTick",
            json!({"tick_instance_id": one, "every_seconds": 5}),
        ),
        text("watching one"),
        call(
            "s2",
            "SubscribeTick",
            json!({"tick_instance_id": two, "every_seconds": 10}),
        ),
        text("watching two"),
        text("handled one"),
        text("handled two"),
        text("two continued"),
    ]));
    let host = Arc::new(TerminalHost::new());
    let access = Arc::new(TickAccess::with_host(
        HerdrContext {
            socket: "/tmp/herdr-test.sock".into(),
            caller_pane: "agent".into(),
        },
        host.clone(),
    ));
    let agent = Agent::new_embedded(config(), model.clone());
    agent
        .install_tick_stream(Arc::new(NativeTickStream::new(access)))
        .await;
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    let mut events = agent.subscribe();
    agent.submit(chat, None, "watch one".into()).await.unwrap();
    completed(&mut events).await;
    agent.submit(chat, None, "watch two".into()).await.unwrap();
    completed(&mut events).await;
    settle().await;
    assert_eq!(
        model.requests.lock().unwrap().len(),
        4,
        "initial terminal text is only a baseline"
    );

    tokio::time::advance(Duration::from_secs(10)).await;
    settle().await;
    assert_eq!(
        model.requests.lock().unwrap().len(),
        4,
        "unchanged terminal text does not trigger a loop"
    );

    host.set("one", "changed one");
    host.set("two", "changed two");
    tokio::time::advance(Duration::from_secs(5)).await;
    settle().await;
    tokio::time::advance(Duration::from_secs(5)).await;
    settle().await;
    completed(&mut events).await;
    completed(&mut events).await;
    {
        let requests = model.requests.lock().unwrap();
        assert_eq!(requests.len(), 6);
        let delivered: Vec<_> = requests[4..6]
            .iter()
            .map(|request| {
                request
                    .messages
                    .iter()
                    .rev()
                    .find_map(|message| {
                        (message.role == tickloop_agent::model::Role::User)
                            .then(|| {
                                message.content.iter().find_map(|content| match content {
                                    tickloop_agent::model::Content::Text { text } => {
                                        Some(text.clone())
                                    }
                                    _ => None,
                                })
                            })
                            .flatten()
                    })
                    .unwrap()
            })
            .collect();
        assert!(
            delivered
                .iter()
                .any(|text| text.contains("changed one") && !text.contains("changed two")),
            "{delivered:?}"
        );
        assert!(
            delivered
                .iter()
                .any(|text| text.contains("changed two") && !text.contains("changed one")),
            "{delivered:?}"
        );
    }
    let rows = agent.tick_subscriptions(chat).await.unwrap();
    assert!(rows.iter().all(|row| row.link == TickLinkState::Live));
    assert!(
        agent
            .snapshot(chat)
            .await
            .unwrap()
            .loops
            .iter()
            .all(|loop_| loop_.state == LoopState::WaitingTrigger)
    );

    *host.gone.lock().unwrap() = Some("one".into());
    host.set("two", "two after one exited");
    tokio::time::advance(Duration::from_secs(5)).await;
    settle().await;
    tokio::time::advance(Duration::from_secs(10)).await;
    settle().await;
    completed(&mut events).await;
    assert!(last_user_text(&model).contains("two after one exited"));
    let rows = agent.tick_subscriptions(chat).await.unwrap();
    assert!(
        rows.iter()
            .any(|row| row.instance == one && row.link == TickLinkState::InstanceGone)
    );
    assert!(
        rows.iter()
            .any(|row| row.instance == two && row.link == TickLinkState::Live)
    );
    agent.shutdown().await;
}

#[tokio::test(start_paused = true)]
async fn terminal_host_outage_requires_reselection_after_event_continuity_is_lost() {
    let directory = tempfile::tempdir().unwrap();
    let instance = instance("one");
    let model = Arc::new(ScriptedModel::new(vec![
        call(
            "subscribe",
            "SubscribeTick",
            json!({"tick_instance_id":instance,"every_seconds":5}),
        ),
        text("watching"),
        call(
            "subscribe-again",
            "SubscribeTick",
            json!({"tick_instance_id":instance,"every_seconds":5}),
        ),
        text("watching after explicit reselection"),
    ]));
    let host = Arc::new(TerminalHost::new());
    let access = Arc::new(TickAccess::with_host(
        HerdrContext {
            socket: "/tmp/herdr-test.sock".into(),
            caller_pane: "agent".into(),
        },
        host.clone(),
    ));
    let agent = Agent::new_embedded(config(), model.clone());
    agent
        .install_tick_stream(Arc::new(NativeTickStream::new(access)))
        .await;
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    let mut events = agent.subscribe();
    agent.submit(chat, None, "watch one".into()).await.unwrap();
    completed(&mut events).await;
    settle().await;
    assert_eq!(
        agent.tick_subscriptions(chat).await.unwrap()[0].link,
        TickLinkState::Live
    );

    host.outage.store(true, Ordering::SeqCst);
    tokio::time::advance(Duration::from_secs(5)).await;
    settle().await;
    assert_eq!(
        agent.tick_subscriptions(chat).await.unwrap()[0].link,
        TickLinkState::Disconnected
    );
    host.outage.store(false, Ordering::SeqCst);
    host.set("one", "same process but events may have been missed");
    tokio::time::advance(Duration::from_secs(5)).await;
    settle().await;
    assert_eq!(
        agent.tick_subscriptions(chat).await.unwrap()[0].link,
        TickLinkState::IdentityUncertain
    );
    tokio::time::advance(Duration::from_secs(10)).await;
    settle().await;
    assert_eq!(
        model.requests.lock().unwrap().len(),
        2,
        "unproven continuity must not resume this Loop"
    );
    agent
        .submit(chat, None, "explicitly reselect the proven run".into())
        .await
        .unwrap();
    completed(&mut events).await;
    settle().await;
    let rows = agent.tick_subscriptions(chat).await.unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].link, TickLinkState::IdentityUncertain);
    assert_eq!(rows[1].link, TickLinkState::Live);
    agent.shutdown().await;
}

#[tokio::test(start_paused = true)]
async fn a_new_watch_restarts_a_retired_native_worker_without_rearming_its_old_watch() {
    let host = Arc::new(TerminalHost::new());
    let access = Arc::new(TickAccess::with_host(
        HerdrContext {
            socket: "/tmp/herdr-test.sock".into(),
            caller_pane: "agent".into(),
        },
        host.clone(),
    ));
    let mut stream = NativeTickStream::new(access).open();
    let target = instance("one");
    let old = Uuid::new_v4();
    stream
        .desired
        .send(BTreeMap::from([(old, target.clone())]))
        .unwrap();
    settle().await;
    assert!(
        stream
            .events
            .try_recv()
            .is_ok_and(|event| matches!(event, TickStreamEvent::Ready { watch } if watch == old))
    );

    host.outage.store(true, Ordering::SeqCst);
    tokio::time::advance(Duration::from_secs(5)).await;
    settle().await;
    host.outage.store(false, Ordering::SeqCst);
    host.set("one", "different text after the outage");
    tokio::time::advance(Duration::from_secs(5)).await;
    settle().await;
    let mut uncertain = false;
    while let Ok(event) = stream.events.try_recv() {
        uncertain |= matches!(
            event,
            TickStreamEvent::State {
                state: TickConnectionState::IdentityUncertain,
                ..
            }
        );
    }
    assert!(uncertain);

    let new = Uuid::new_v4();
    stream
        .desired
        .send(BTreeMap::from([(old, target.clone()), (new, target)]))
        .unwrap();
    settle().await;
    let mut ready = Vec::new();
    while let Ok(event) = stream.events.try_recv() {
        if let TickStreamEvent::Ready { watch } = event {
            ready.push(watch);
        }
    }
    assert_eq!(ready, vec![new]);
}

#[tokio::test(start_paused = true)]
async fn a_finished_old_read_cannot_block_a_fresh_watch_that_already_has_its_baseline() {
    let host = Arc::new(BlockingTerminalHost {
        inner: TerminalHost::new(),
        reads: AtomicUsize::new(0),
        blocked: tokio::sync::Notify::new(),
        release: tokio::sync::Notify::new(),
    });
    let access = Arc::new(TickAccess::with_host(
        HerdrContext {
            socket: "/tmp/herdr-test.sock".into(),
            caller_pane: "agent".into(),
        },
        host.clone(),
    ));
    let mut stream = NativeTickStream::new(access).open();
    let target = instance("one");
    let old = Uuid::new_v4();
    stream
        .desired
        .send(BTreeMap::from([(old, target.clone())]))
        .unwrap();
    let old_ready = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if let Some(TickStreamEvent::Ready { watch }) = stream.events.recv().await {
                break watch;
            }
        }
    })
    .await
    .expect("old Watch did not reach baseline");
    assert_eq!(old_ready, old);

    tokio::time::advance(Duration::from_secs(5)).await;
    tokio::time::timeout(Duration::from_secs(2), host.blocked.notified())
        .await
        .expect("old worker did not start its second read");
    stream.desired.send(BTreeMap::new()).unwrap();
    settle().await;
    let fresh = Uuid::new_v4();
    stream
        .desired
        .send(BTreeMap::from([(fresh, target)]))
        .unwrap();
    let fresh_ready = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if let Some(TickStreamEvent::Ready { watch }) = stream.events.recv().await {
                break watch;
            }
        }
    })
    .await
    .expect("fresh Watch did not reach baseline");
    assert_eq!(fresh_ready, fresh);
    host.release.notify_one();
    settle().await;

    while let Ok(event) = stream.events.try_recv() {
        assert!(
            !matches!(&event, TickStreamEvent::State { audience, state: TickConnectionState::IdentityUncertain, .. } if audience.contains(&fresh)),
            "old worker's failed read must not retire the fresh Watch: {event:?}"
        );
    }
    host.inner
        .set("one", "fresh content after the old read failed");
    tokio::time::advance(Duration::from_secs(5)).await;
    settle().await;
    let mut saw_fresh = false;
    while let Ok(event) = stream.events.try_recv() {
        saw_fresh |= matches!(event, TickStreamEvent::Observation { audience, value } if audience.contains(&fresh) && value.data["text"] == "fresh content after the old read failed");
    }
    assert!(saw_fresh, "fresh Watch must keep observing its own worker");
}

#[tokio::test(start_paused = true)]
async fn a_watch_joining_during_an_old_read_survives_the_old_watch_terminal_state() {
    let host = Arc::new(BlockingTerminalHost {
        inner: TerminalHost::new(),
        reads: AtomicUsize::new(0),
        blocked: tokio::sync::Notify::new(),
        release: tokio::sync::Notify::new(),
    });
    let access = Arc::new(TickAccess::with_host(
        HerdrContext {
            socket: "/tmp/herdr-test.sock".into(),
            caller_pane: "agent".into(),
        },
        host.clone(),
    ));
    let mut stream = NativeTickStream::new(access).open();
    let target = instance("one");
    let old = Uuid::new_v4();
    stream
        .desired
        .send(BTreeMap::from([(old, target.clone())]))
        .unwrap();
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if let Some(TickStreamEvent::Ready { watch }) = stream.events.recv().await {
                assert_eq!(watch, old);
                break;
            }
        }
    })
    .await
    .expect("old Watch did not reach baseline");

    tokio::time::advance(Duration::from_secs(5)).await;
    tokio::time::timeout(Duration::from_secs(2), host.blocked.notified())
        .await
        .expect("old worker did not start its second read");
    let fresh = Uuid::new_v4();
    stream
        .desired
        .send(BTreeMap::from([(old, target.clone()), (fresh, target)]))
        .unwrap();
    settle().await;
    host.release.notify_one();

    let mut saw_old_terminal = false;
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let event = stream
                .events
                .recv()
                .await
                .expect("Native event stream closed");
            match event {
                TickStreamEvent::State {
                    audience,
                    state: TickConnectionState::IdentityUncertain,
                    ..
                } => {
                    assert!(
                        !audience.contains(&fresh),
                        "old failure blocked fresh Watch"
                    );
                    saw_old_terminal |= audience.contains(&old);
                }
                TickStreamEvent::Ready { watch } if watch == fresh => break,
                _ => {}
            }
        }
    })
    .await
    .expect("fresh Watch did not establish a new baseline");
    assert!(saw_old_terminal, "old Watch's terminal state was lost");
    settle().await;
    while let Ok(event) = stream.events.try_recv() {
        assert!(
            !matches!(&event, TickStreamEvent::State { audience, state: TickConnectionState::IdentityUncertain, .. } if audience.contains(&fresh)),
            "late old failure blocked fresh Watch: {event:?}"
        );
    }
    host.inner
        .set("one", "fresh watch after old terminal state");
    tokio::time::advance(Duration::from_secs(5)).await;
    settle().await;
    let mut saw_fresh = false;
    while let Ok(event) = stream.events.try_recv() {
        saw_fresh |= matches!(event, TickStreamEvent::Observation { audience, value } if audience.contains(&fresh) && value.data["text"] == "fresh watch after old terminal state");
    }
    assert!(
        saw_fresh,
        "fresh Watch must continue after the old Watch stops"
    );
}

#[tokio::test(start_paused = true)]
async fn a_retired_native_worker_keeps_its_final_state_for_a_slow_consumer() {
    let host = Arc::new(TerminalHost::new());
    let access = Arc::new(TickAccess::with_host(
        HerdrContext {
            socket: "/tmp/herdr-test.sock".into(),
            caller_pane: "agent".into(),
        },
        host.clone(),
    ));
    let mut stream = NativeTickStream::new(access).open();
    let target = instance("one");
    let watches: BTreeMap<_, _> = (0..130).map(|_| (Uuid::new_v4(), target.clone())).collect();
    stream.desired.send(watches).unwrap();
    settle().await;

    host.outage.store(true, Ordering::SeqCst);
    tokio::time::advance(Duration::from_secs(5)).await;
    settle().await;
    host.outage.store(false, Ordering::SeqCst);
    host.set("one", "text changed while the host was unavailable");
    tokio::time::advance(Duration::from_secs(5)).await;
    settle().await;

    let mut saw_final_state = false;
    for _ in 0..160 {
        if let Ok(event) = stream.events.try_recv() {
            saw_final_state |= matches!(
                event,
                TickStreamEvent::State {
                    state: TickConnectionState::IdentityUncertain,
                    ..
                }
            );
        }
        tokio::task::yield_now().await;
    }
    assert!(
        saw_final_state,
        "the final state survives mailbox backpressure"
    );
}

#[tokio::test]
async fn protocol_tick_update_after_registration_reaches_its_loop() {
    let directory = tempfile::Builder::new()
        .prefix("tlp")
        .tempdir_in("/private/tmp")
        .unwrap();
    let runtime_dir = directory.path().join("sessions");
    let host_socket = directory.path().join("herdr.sock");
    let script = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../examples/ticks/counter.py");
    let session = TickSession::spawn(SessionLaunch {
        command: vec!["python3".into(), script.to_string_lossy().into_owned()],
        cwd: directory.path().into(),
        host_socket: host_socket.clone(),
        terminal_id: "protocol".into(),
        runtime_dir: Some(runtime_dir.clone()),
    })
    .await
    .unwrap();
    let instance = session.instance().clone();
    let host = Arc::new(TerminalHost::new());
    *host.protocol.lock().unwrap() = Some(instance.clone());
    let access = Arc::new(
        TickAccess::with_host(
            HerdrContext {
                socket: host_socket,
                caller_pane: "agent".into(),
            },
            host,
        )
        .with_runtime_dir(runtime_dir),
    );
    let model = Arc::new(ScriptedModel::new(vec![
        call(
            "subscribe",
            "SubscribeTick",
            json!({"tick_instance_id":instance,"every_seconds":5}),
        ),
        text("watching counter"),
        text("counter moved"),
    ]));
    let agent = Agent::new_embedded(config(), model.clone());
    agent
        .install_tick_stream(Arc::new(NativeTickStream::new(access)))
        .await;
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    let mut events = agent.subscribe();
    agent
        .submit(chat, None, "watch the counter".into())
        .await
        .unwrap();
    completed(&mut events).await;
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if agent.tick_subscriptions(chat).await.unwrap()[0].link == TickLinkState::Live {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(
        model.requests.lock().unwrap().len(),
        2,
        "initial content is a baseline"
    );

    session
        .client()
        .call("increment", json!({"amount":5}), CancellationToken::new())
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if matches!(
                events.recv().await.unwrap().kind,
                EventKind::Completed { .. }
            ) {
                break;
            }
        }
    })
    .await
    .unwrap();
    let delivered = last_user_text(&model);
    assert!(
        delivered.contains("\\\"value\\\":5") || delivered.contains("\"value\":5"),
        "{delivered}"
    );
    assert!(delivered.contains(&instance.to_string()), "{delivered}");
    agent.shutdown().await;
    session.shutdown().await.unwrap();
}

#[tokio::test]
async fn protocol_sequence_gap_is_reported_separately_from_received_observation_drops() {
    let directory = tempfile::Builder::new()
        .prefix("tlp")
        .tempdir_in("/private/tmp")
        .unwrap();
    let runtime_dir = directory.path().join("sessions");
    let host_socket = directory.path().join("herdr.sock");
    let script = directory.path().join("sequence_gap.py");
    std::fs::write(&script, r#"
import json, os, sys
instance = os.environ['TICKLOOP_INSTANCE_ID']
sequence = 0
for line in sys.stdin:
    request = json.loads(line)
    method = request['method']
    if method == 'tick/describe':
        result = {'protocol':2,'tick_type':'example.gap','title':'Gap','description':'Sequence gap fixture','recover_with':'current','tools':[{'name':'jump','description':'Jump sequence','params':{'type':'object'},'mutates':True},{'name':'current','description':'Read current','params':{'type':'object'},'mutates':False}]}
    elif method == 'tick/read':
        result = {'version':3,'tick_instance_id':instance,'initialized':True,'sequence':sequence,'observed_at':'2026-09-24T00:00:00Z','data':{'value':sequence}}
    elif method == 'tool/call':
        if request['params']['tool'] == 'jump':
            sequence += 3
            print(json.dumps({'jsonrpc':'2.0','method':'tick/update','params':{'version':3,'tick_instance_id':instance,'source':'example.gap','sequence':sequence,'observed_at':'2026-09-24T00:00:01Z','data':{'value':sequence}}}),flush=True)
        result = {'value':sequence}
    print(json.dumps({'jsonrpc':'2.0','id':request['id'],'result':result}),flush=True)
"#).unwrap();
    let session = TickSession::spawn(SessionLaunch {
        command: vec![
            "python3".into(),
            "-u".into(),
            script.to_string_lossy().into_owned(),
        ],
        cwd: directory.path().into(),
        host_socket: host_socket.clone(),
        terminal_id: "protocol".into(),
        runtime_dir: Some(runtime_dir.clone()),
    })
    .await
    .unwrap();
    let instance = session.instance().clone();
    let host = Arc::new(TerminalHost::new());
    *host.protocol.lock().unwrap() = Some(instance.clone());
    let access = Arc::new(
        TickAccess::with_host(
            HerdrContext {
                socket: host_socket,
                caller_pane: "agent".into(),
            },
            host,
        )
        .with_runtime_dir(runtime_dir),
    );
    let model = Arc::new(ScriptedModel::new(vec![
        call(
            "subscribe",
            "SubscribeTick",
            json!({"tick_instance_id":instance,"every_seconds":5}),
        ),
        text("watching gap fixture"),
        text("reported gap"),
    ]));
    let agent = Agent::new_embedded(config(), model.clone());
    agent
        .install_tick_stream(Arc::new(NativeTickStream::new(access)))
        .await;
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    let mut events = agent.subscribe();
    agent
        .submit(chat, None, "watch the fixture".into())
        .await
        .unwrap();
    completed(&mut events).await;
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if agent.tick_subscriptions(chat).await.unwrap()[0].link == TickLinkState::Live {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    session
        .client()
        .call("jump", json!({}), CancellationToken::new())
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if matches!(
                events.recv().await.unwrap().kind,
                EventKind::Completed { .. }
            ) {
                break;
            }
        }
    })
    .await
    .unwrap();
    let delivered = last_user_text(&model);
    assert!(delivered.contains("\"sequence\":3"), "{delivered}");
    assert!(delivered.contains("\"gap\":true"), "{delivered}");
    assert!(delivered.contains("\"dropped\":0"), "{delivered}");
    agent.shutdown().await;
    session.shutdown().await.unwrap();
}

#[tokio::test]
async fn herdr_adapter_aliases_keep_native_query_permissions_and_target_admission() {
    use support::tick_alias_contract::{AliasCase, verify_alias_policy};
    use tickloop_agent::{
        host_tools::HostToolProvider,
        native_host::NativeHostToolProvider,
        tick_adapter::{HerdrAdapter, TickAdapter, TickAdapters},
    };
    let directory = tempfile::Builder::new()
        .prefix("ta-contract")
        .tempdir_in("/private/tmp")
        .unwrap();
    let runtime_dir = directory.path().join("sessions");
    let host_socket = directory.path().join("herdr.sock");
    let script = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../examples/ticks/counter.py");
    let session = TickSession::spawn(SessionLaunch {
        command: vec!["python3".into(), script.to_string_lossy().into_owned()],
        cwd: directory.path().into(),
        host_socket: host_socket.clone(),
        terminal_id: "protocol".into(),
        runtime_dir: Some(runtime_dir.clone()),
    })
    .await
    .unwrap();
    let instance = session.instance().clone();
    let host = Arc::new(TerminalHost::new());
    *host.protocol.lock().unwrap() = Some(instance.clone());
    let access = Arc::new(
        TickAccess::with_host(
            HerdrContext {
                socket: host_socket,
                caller_pane: "agent".into(),
            },
            host.clone(),
        )
        .with_runtime_dir(runtime_dir),
    );
    let native = NativeHostToolProvider::new(access.clone());
    let adapters = Arc::new(
        TickAdapters::new([Arc::new(HerdrAdapter::new(Some(access))) as Arc<dyn TickAdapter>])
            .unwrap(),
    );
    let model = Arc::new(ScriptedModel::new(vec![]));
    let agent = Agent::new_embedded(config(), model.clone());
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    let read_input = json!({"tick_instance_id":instance});
    let query_input = json!({"tick_instance_id":instance,"tool":"history","args":{}});
    let read = native
        .prepare_call(chat, "ReadTick", &read_input, CancellationToken::new())
        .await
        .unwrap();
    let query = native
        .prepare_call(chat, "CallTickTool", &query_input, CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(
        read.permission(),
        tickloop_agent::host_tools::HostToolPermission::ReadOnly
    );
    assert_eq!(query.permission(), read.permission());
    let mutation_input =
        json!({"tick_instance_id":instance,"tool":"increment","args":{"amount":1}});
    let mutation = native
        .prepare_call(
            chat,
            "CallTickTool",
            &mutation_input,
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(
        mutation.permission(),
        tickloop_agent::host_tools::HostToolPermission::Reviewable
    );
    let cases = [
        AliasCase {
            plan_denied: true,
            name: "CallTickTool",
            input: mutation_input,
            policy_name: "CallTickTool",
            policy_input: mutation.input().clone(),
        },
        AliasCase {
            plan_denied: false,
            name: "ReadTick",
            input: read_input.clone(),
            policy_name: "ReadTick",
            policy_input: read.input().clone(),
        },
        AliasCase {
            plan_denied: false,
            name: "CallTickTool",
            input: query_input.clone(),
            policy_name: "CallTickTool",
            policy_input: query.input().clone(),
        },
        AliasCase {
            plan_denied: false,
            name: "SubscribeTick",
            input: json!({"tick_instance_id":instance,"every_seconds":5}),
            policy_name: "ReadTick",
            policy_input: read.input().clone(),
        },
    ];
    let tools = adapters.host_tools(None);
    agent.install_host_tools(tools.clone()).unwrap();
    agent.install_tick_stream(adapters.clone()).await;
    verify_alias_policy(&agent, &model, chat, &cases).await;
    session.shutdown().await.unwrap();
    *host.protocol.lock().unwrap() = None;
    assert!(
        native
            .prepare_call(chat, "ReadTick", &read_input, CancellationToken::new())
            .await
            .is_err()
    );
    for (name, input) in [("ReadTick", read_input), ("CallTickTool", query_input)] {
        assert!(
            tools
                .prepare_call(chat, name, &input, CancellationToken::new())
                .await
                .is_err()
        );
    }
    assert!(
        adapters
            .prepare_watch(chat, &instance, CancellationToken::new())
            .await
            .is_err()
    );
    agent.shutdown().await;
}
