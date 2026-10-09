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
use support::{CONNECTS_PER_WATCH, ScriptedModel, call, config, next_event, text};
use tick_connect::{
    access::TickAccess,
    herdr::{HerdrContext, HerdrPort, Pane, process_birth},
    host_error::HostError,
    instance::InstanceRef,
    session::{SessionLaunch, TickSession},
};
use tickloop_agent::{
    runtime::{Agent, Event, EventKind, LoopState, TickLinkState},
    tick_stream::{
        NativeTickStream, RECONNECT_PAUSE, RECONNECTS_PER_WATCH, TickConnectionState,
        TickStreamEvent, TickStreamPort,
    },
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
    /// Requests refused while the controlled connection was down.
    refused: AtomicUsize,
    content_reads: AtomicUsize,
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
            refused: AtomicUsize::new(0),
            content_reads: AtomicUsize::new(0),
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
            self.refused.fetch_add(1, Ordering::SeqCst);
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
                self.content_reads.fetch_add(1, Ordering::SeqCst);
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

fn native_provider(
    access: Arc<TickAccess>,
) -> Arc<tickloop_agent::native_host::NativeHostToolProvider> {
    Arc::new(
        tickloop_agent::native_host::NativeHostToolProvider::with_trusted_adapter_identity(
            access,
            Uuid::new_v4(),
        ),
    )
}
fn bind_watches(
    stream: &NativeTickStream,
    provider: &tickloop_agent::native_host::NativeHostToolProvider,
    watches: impl IntoIterator<Item = Uuid>,
) {
    use tickloop_agent::host_tools::HostToolProvider;
    let authority = provider
        .tool_authorities("ReadTick")
        .unwrap()
        .pop_first()
        .unwrap();
    stream.set_watch_authorities(
        watches
            .into_iter()
            .map(|watch| (watch, authority.clone()))
            .collect(),
    );
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
        .install_tick_stream(Arc::new(NativeTickStream::new(native_provider(access))))
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
        .install_tick_stream(Arc::new(NativeTickStream::new(native_provider(access))))
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
    let provider = native_provider(access);
    let native = NativeTickStream::new(provider.clone());
    let mut stream = native.open();
    let target = instance("one");
    let old = Uuid::new_v4();
    bind_watches(&native, &provider, [old]);
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
    bind_watches(&native, &provider, [new]);
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
    let provider = native_provider(access);
    let native = NativeTickStream::new(provider.clone());
    let mut stream = native.open();
    let target = instance("one");
    let old = Uuid::new_v4();
    bind_watches(&native, &provider, [old]);
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
    bind_watches(&native, &provider, [old, fresh]);
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
    let provider = native_provider(access);
    let native = NativeTickStream::new(provider.clone());
    let mut stream = native.open();
    let target = instance("one");
    let old = Uuid::new_v4();
    bind_watches(&native, &provider, [old]);
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
    bind_watches(&native, &provider, [old, fresh]);
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
    let provider = native_provider(access);
    let native = NativeTickStream::new(provider.clone());
    let mut stream = native.open();
    let target = instance("one");
    let watches: BTreeMap<_, _> = (0..130).map(|_| (Uuid::new_v4(), target.clone())).collect();
    bind_watches(&native, &provider, watches.keys().copied());
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
        backend: tick_connect::session::SessionBackend::Herdr {
            socket: host_socket.clone(),
            terminal_id: "protocol".into(),
        },
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
        .install_tick_stream(Arc::new(NativeTickStream::new(native_provider(access))))
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
        backend: tick_connect::session::SessionBackend::Herdr {
            socket: host_socket.clone(),
            terminal_id: "protocol".into(),
        },
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
        .install_tick_stream(Arc::new(NativeTickStream::new(native_provider(access))))
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
async fn herdr_subscriptions_keep_native_query_permissions_and_target_admission() {
    use support::tick_admission_contract::{AdmissionCase, verify_admission_policy};
    use tickloop_agent::{
        host_tools::HostToolProvider,
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
        backend: tick_connect::session::SessionBackend::Herdr {
            socket: host_socket.clone(),
            terminal_id: "protocol".into(),
        },
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
    let native = native_provider(access.clone());
    let adapters = Arc::new(
        TickAdapters::new([
            Arc::new(HerdrAdapter::new(Some(native.clone()))) as Arc<dyn TickAdapter>
        ])
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
        AdmissionCase {
            plan_denied: true,
            name: "CallTickTool",
            input: mutation_input,
            policy_name: "CallTickTool",
            policy_input: mutation.input().clone(),
        },
        AdmissionCase {
            plan_denied: false,
            name: "ReadTick",
            input: read_input.clone(),
            policy_name: "ReadTick",
            policy_input: read.input().clone(),
        },
        AdmissionCase {
            plan_denied: false,
            name: "CallTickTool",
            input: query_input.clone(),
            policy_name: "CallTickTool",
            policy_input: query.input().clone(),
        },
        AdmissionCase {
            plan_denied: false,
            name: "SubscribeTick",
            input: json!({"tick_instance_id":instance,"every_seconds":5}),
            policy_name: "ReadTick",
            policy_input: read.input().clone(),
        },
    ];
    let tools = native.clone();
    agent.install_host_tools(tools.clone()).unwrap();
    agent.install_tick_stream(adapters.clone()).await;
    verify_admission_policy(&agent, &model, chat, &cases).await;
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
            .prepare_watch(
                chat,
                &instance,
                &json!({"tick_instance_id":instance,"every_seconds":5}),
                CancellationToken::new()
            )
            .await
            .is_err()
    );
    agent.shutdown().await;
}

async fn socket_fixture(
    path: std::path::PathBuf,
    host: Arc<TerminalHost>,
) -> tokio::task::JoinHandle<()> {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    let listener = tokio::net::UnixListener::bind(path).unwrap();
    tokio::spawn(async move {
        loop {
            let (socket, _) = listener.accept().await.unwrap();
            let host = host.clone();
            tokio::spawn(async move {
                let mut socket = BufReader::new(socket);
                let mut line = String::new();
                socket.read_line(&mut line).await.unwrap();
                let request: Value = serde_json::from_str(&line).unwrap();
                let result = host
                    .request(
                        request["method"].as_str().unwrap(),
                        request["params"].clone(),
                        CancellationToken::new(),
                    )
                    .await
                    .unwrap();
                let response = json!({"id":request["id"],"result":result});
                socket
                    .get_mut()
                    .write_all(format!("{response}\n").as_bytes())
                    .await
                    .unwrap();
            });
        }
    })
}

#[tokio::test]
async fn restored_native_watch_refuses_a_changed_frozen_endpoint() {
    use tickloop_agent::{
        host_tools::{HostCallContext, HostToolProvider},
        native_host::NativeHostToolProvider,
        permissions::ExecutionMode,
    };
    let directory = tempfile::tempdir_in("/tmp").unwrap();
    let root = directory.path().canonicalize().unwrap();
    let store = root.join("store");
    let first = root.join("first.sock");
    let second = root.join("second.sock");
    let host_a = Arc::new(TerminalHost::new());
    let host_b = Arc::new(TerminalHost::new());
    let server_a = socket_fixture(first.clone(), host_a).await;
    let server_b = socket_fixture(second.clone(), host_b.clone()).await;
    let access_a = Arc::new(TickAccess::new(HerdrContext {
        socket: first,
        caller_pane: "agent".into(),
    }));
    let access_b = Arc::new(TickAccess::new(HerdrContext {
        socket: second,
        caller_pane: "agent".into(),
    }));
    let instance = instance("one");
    let model = Arc::new(ScriptedModel::new(vec![
        call(
            "parent-subscribe",
            "SubscribeTick",
            json!({"tick_instance_id":instance,"every_seconds":5}),
        ),
        call(
            "delegate",
            "Subagent",
            json!({"task":"Watch the authorized terminal"}),
        ),
        call(
            "child-subscribe",
            "SubscribeTick",
            json!({"tick_instance_id":instance,"every_seconds":5}),
        ),
        text("Child watching"),
        text("Parent watching"),
    ]));
    let agent = Agent::open_embedded(store.clone(), config(), model).unwrap();
    agent
        .install_host_tools(Arc::new(NativeHostToolProvider::new(access_a.clone())))
        .unwrap();
    agent
        .install_tick_stream(Arc::new(NativeTickStream::new(Arc::new(
            tickloop_agent::native_host::NativeHostToolProvider::new(access_a),
        ))))
        .await;
    let parent = agent.create_chat(root.clone()).await.unwrap();
    agent
        .set_execution_mode(parent, ExecutionMode::FullAccess)
        .await
        .unwrap();
    agent
        .set_host_work_context(
            parent,
            HostCallContext {
                workdir: root.clone(),
                workspace_id: Some("workspace".into()),
                interaction: None,
            },
        )
        .await
        .unwrap();
    let mut events = agent.subscribe();
    agent
        .submit(parent, None, "Watch then delegate".into())
        .await
        .unwrap();
    loop {
        let event = next_event(&mut events).await;
        if let EventKind::ToolCompleted { call_id, result } = &event.kind
            && call_id.ends_with("subscribe")
        {
            assert!(!result.is_error, "{}", result.content);
        }
        if event.chat_id == parent && matches!(event.kind, EventKind::Completed { .. }) {
            break;
        }
    }
    let child = agent
        .list_chats()
        .await
        .into_iter()
        .find(|c| c.id != parent)
        .unwrap()
        .id;
    let snapshot = agent.snapshot(child).await.unwrap();
    let frozen = snapshot.delegation.as_ref().unwrap().ceiling.host_tools["ReadTick"].clone();
    assert_eq!(
        frozen.len(),
        1,
        "one Herdr endpoint was present: {frozen:?}"
    );
    let old_authority = frozen.first().unwrap().clone();
    let loop_id = snapshot.loops[0].id;
    assert_eq!(
        snapshot.loops[0].trigger.as_ref().unwrap().subscriptions[0].authority,
        Some(old_authority.clone())
    );
    agent.shutdown().await;
    let provider_b = Arc::new(NativeHostToolProvider::new(access_b.clone()));
    let new_authority = provider_b
        .tool_authorities("ReadTick")
        .unwrap()
        .pop_first()
        .unwrap();
    assert_ne!(old_authority, new_authority);
    let model = Arc::new(ScriptedModel::new(vec![
        call(
            "restored-direct-read",
            "ReadTick",
            json!({"tick_instance_id":instance}),
        ),
        text("Continue watching"),
        text("Unexpected endpoint data received"),
    ]));
    let restored = Agent::open_embedded(store, config(), model.clone()).unwrap();
    restored.install_host_tools(provider_b.clone()).unwrap();
    restored
        .install_tick_stream(Arc::new(NativeTickStream::new(provider_b.clone())))
        .await;
    let snapshot = restored.resume_chat(child).await.unwrap();
    assert_eq!(
        snapshot.delegation.unwrap().ceiling.host_tools["ReadTick"],
        frozen
    );
    let mut events = restored.subscribe();
    restored
        .submit(child, Some(loop_id), "Continue authorized watch".into())
        .await
        .unwrap();
    loop {
        let event = next_event(&mut events).await;
        if let EventKind::ToolCompleted { call_id, result } = &event.kind
            && call_id == "restored-direct-read"
        {
            assert!(
                result.is_error
                    && result
                        .content
                        .contains("outside the child's inherited authorization"),
                "{}",
                result.content
            );
        }
        if matches!(event.kind, EventKind::Completed { .. }) {
            break;
        }
    }
    tokio::time::sleep(Duration::from_secs(2)).await;
    host_b.set("one", "CHANGED_ENDPOINT_SECRET");
    let delivery = tokio::time::timeout(Duration::from_secs(12), async {
        loop {
            let event = events.recv().await.unwrap();
            if event.chat_id == child && matches!(event.kind, EventKind::Completed { .. }) {
                return last_user_text(&model);
            }
        }
    })
    .await;

    assert_eq!(
        host_b.content_reads.load(Ordering::SeqCst),
        0,
        "restored watch sampled its changed endpoint before rejection"
    );
    restored.shutdown().await;
    server_a.abort();
    server_b.abort();
    assert!(
        delivery.is_err(),
        "changed endpoint reached frozen child: {delivery:?}"
    );
}

#[tokio::test]
async fn native_stream_rejects_missing_or_foreign_bindings_before_any_host_read() {
    use tickloop_agent::host_tools::{HostEndpointIdentity, HostToolAuthority, HostToolProvider};
    let host = Arc::new(TerminalHost::new());
    let access = Arc::new(TickAccess::with_host(
        HerdrContext {
            socket: "/tmp/controlled-test.sock".into(),
            caller_pane: "agent".into(),
        },
        host.clone(),
    ));
    let provider = native_provider(access);
    for binding in [
        None,
        Some(HostToolAuthority::Native {
            endpoint: HostEndpointIdentity::TrustedAdapter {
                scope: Uuid::new_v4(),
            },
        }),
    ] {
        let stream = NativeTickStream::new(provider.clone());
        let watch = Uuid::new_v4();
        stream.set_watch_authorities(
            binding
                .clone()
                .map(|authority| BTreeMap::from([(watch, authority)]))
                .unwrap_or_default(),
        );
        let mut handle = stream.open();
        let target = instance("one");
        assert!(
            stream
                .revalidate_watch(Uuid::new_v4(), &target, binding.as_ref())
                .await
                .is_err()
        );
        handle
            .desired
            .send(BTreeMap::from([(watch, target)]))
            .unwrap();
        let event = tokio::time::timeout(Duration::from_secs(2), handle.events.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(
            matches!(
                event,
                TickStreamEvent::State {
                    state: TickConnectionState::IdentityUncertain,
                    ..
                }
            ),
            "a foreign/unbound watch reached baseline or content: {event:?}"
        );
        handle.desired.send(BTreeMap::new()).unwrap();
    }
    assert_eq!(
        host.content_reads.load(Ordering::SeqCst),
        0,
        "unbound or foreign Native watch read host content"
    );
    assert!(!provider.tool_authorities("ReadTick").unwrap().is_empty());
}

/// A native watch whose Herdr stays unreachable connects once and reconnects three times, then
/// stays Disconnected even after Herdr answers again. Only a new watch of the same pane, as
/// subscribing again makes, reaches it.
#[tokio::test(start_paused = true)]
async fn a_native_watch_that_used_its_reconnects_stays_disconnected_until_watched_again() {
    /// Pauses the outage lasts: three times what the watch's connections could use.
    const OUTAGE_PAUSES: usize = 3 * CONNECTS_PER_WATCH;
    assert_eq!(
        (RECONNECTS_PER_WATCH, RECONNECT_PAUSE),
        (3, Duration::from_secs(5)),
        "the production reconnect budget"
    );
    let host = Arc::new(TerminalHost::new());
    host.outage.store(true, Ordering::SeqCst);
    let access = Arc::new(TickAccess::with_host(
        HerdrContext {
            socket: "/tmp/herdr-test.sock".into(),
            caller_pane: "agent".into(),
        },
        host.clone(),
    ));
    let provider = native_provider(access);
    let native = NativeTickStream::new(provider.clone());
    let mut stream = native.open();
    let (spent, fresh) = (Uuid::new_v4(), Uuid::new_v4());
    let one = instance("one");
    bind_watches(&native, &provider, [spent, fresh]);
    stream
        .desired
        .send(BTreeMap::from([(spent, one.clone())]))
        .unwrap();
    for _ in 0..OUTAGE_PAUSES {
        settle().await;
        tokio::time::advance(RECONNECT_PAUSE).await;
    }
    settle().await;
    assert_eq!(
        host.refused.load(Ordering::SeqCst),
        CONNECTS_PER_WATCH,
        "the watch connects once and reconnects three times"
    );

    host.outage.store(false, Ordering::SeqCst);
    for _ in 0..CONNECTS_PER_WATCH {
        tokio::time::advance(RECONNECT_PAUSE).await;
        settle().await;
    }
    assert_eq!(
        host.content_reads.load(Ordering::SeqCst),
        0,
        "a watch out of reconnects never reads the recovered pane"
    );
    let mut spent_states = Vec::new();
    while let Ok(event) = stream.events.try_recv() {
        match event {
            TickStreamEvent::Ready { watch } => assert_ne!(watch, spent, "never ready"),
            TickStreamEvent::State {
                state, audience, ..
            } if audience.contains(&spent) => spent_states.push(state),
            _ => {}
        }
    }
    assert_eq!(
        spent_states.last(),
        Some(&TickConnectionState::Disconnected),
        "{spent_states:?}"
    );

    stream
        .desired
        .send(BTreeMap::from([(spent, one.clone()), (fresh, one)]))
        .unwrap();
    let fresh_ready = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            match stream.events.recv().await.unwrap() {
                TickStreamEvent::Ready { watch } if watch == fresh => return,
                TickStreamEvent::Ready { watch } => panic!("{watch} became ready"),
                _ => {}
            }
        }
    })
    .await;
    assert!(fresh_ready.is_ok(), "a new watch reaches the pane");
    assert!(host.content_reads.load(Ordering::SeqCst) > 0);
}
