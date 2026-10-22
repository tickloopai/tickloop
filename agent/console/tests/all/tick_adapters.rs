use serde_json::json;
use std::{sync::Arc, time::Duration};
use tick_protocol::InstanceRef;
use tickloop_agent_console::monitor_rows::{
    MonitorAction, MonitorContext, MonitorRows, MonitorRowsRegistry, NextCheck, SubscriptionMark,
    TickDescriptor, TickGlyph, TickGroup, TickIntent,
};
use tickloop_agent_protocol::{
    EventKind, ObservationKind, TickSubscriptionChange, TickSubscriptionFrequency,
};
use tickloop_agent_runtime::{
    Agent, ShellTaskCenter,
    tick_adapter::{ShellAdapter, TickAdapter, TickAdapters},
    tick_stream::{InMemoryTickStream, TickObservation, TickStreamPort},
};
use tickloop_agent_test_support::*;
use uuid::Uuid;

struct CalendarAdapter {
    stream: InMemoryTickStream,
}
impl TickAdapter for CalendarAdapter {
    fn access(&self) -> &'static str {
        "cal"
    }
    fn stream(&self) -> Option<Arc<dyn TickStreamPort>> {
        Some(Arc::new(self.stream.clone()))
    }
}
struct CalendarRows;
impl MonitorRows for CalendarRows {
    fn access(&self) -> &'static str {
        "cal"
    }
    fn list(&self, _: &MonitorContext) -> Vec<TickDescriptor> {
        vec![TickDescriptor {
            instance: calendar(),
            group: TickGroup {
                order: 4,
                name: "Calendars",
            },
            glyph: TickGlyph::Protocol,
            title: "Earnings calendar".into(),
            kind: "Calendar".into(),
            status: "live".into(),
            notes: Vec::new(),
            subscription: None,
            owner: tickloop_agent_console::monitor_rows::TickOwner::Unknown,
            tone: tickloop_agent_console::monitor_rows::StatusTone::Neutral,
            updated_at_ms: None,
            update: None,
            resources: None,
            running: true,
            actions: vec![MonitorAction {
                key: 'o',
                label: "open",
                intent: TickIntent::Notify("Calendar opened".into()),
            }],
        }]
    }
}
/// How long a Shell test waits to see that nothing more is delivered: a Shell subscription is
/// immediate, and the Shell Tick looks at its task several times meanwhile.
const SHELL_QUIET_WAIT: Duration = Duration::from_secs(2);

fn calendar() -> InstanceRef {
    InstanceRef::new("cal", "a").unwrap()
}
async fn complete(events: &mut tokio::sync::broadcast::Receiver<tickloop_agent_protocol::Event>) {
    loop {
        if matches!(next_event(events).await.kind, EventKind::Completed { .. }) {
            return;
        }
    }
}
async fn settle() {
    for _ in 0..20 {
        tokio::task::yield_now().await;
    }
}

#[tokio::test(start_paused = true)]
async fn a_registered_type_lists_subscribes_and_triggers_without_type_branches() {
    let directory = tempfile::tempdir().unwrap();
    let stream = InMemoryTickStream::new();
    let adapters = Arc::new(
        TickAdapters::new([Arc::new(CalendarAdapter {
            stream: stream.clone(),
        }) as Arc<dyn TickAdapter>])
        .unwrap(),
    );
    let model = Arc::new(ScriptedModel::new(vec![
        call(
            "s1",
            "TickManage",
            json!({"action":"subscribe","tick_instance_id":calendar()}),
        ),
        text("watching"),
        text("calendar changed"),
    ]));
    let agent = Agent::new_embedded(config(), model.clone());
    agent.install_tick_stream(adapters).await;
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    let mut events = agent.subscribe();
    agent
        .submit(chat, "watch the earnings calendar".into())
        .await
        .unwrap();
    complete(&mut events).await;
    {
        let requests = model.requests.lock().unwrap();
        let subscribe = requests[0]
            .tools
            .iter()
            .find(|tool| tool.name == "TickManage")
            .unwrap();
        assert!(
            jsonschema::validator_for(&subscribe.input_schema)
                .unwrap()
                .is_valid(
                    &json!({"action":"subscribe","tick_instance_id":"cal:a","frequency":"medium"})
                ),
            "the model API must accept a short registered identity"
        );
    }
    let subscriptions = agent.tick_subscriptions(chat).await.unwrap();
    assert_eq!(subscriptions.len(), 1);
    let monitor =
        MonitorRowsRegistry::new([Arc::new(CalendarRows) as Arc<dyn MonitorRows>]).unwrap();
    let rows = monitor.list(&MonitorContext {
        subscriptions: subscriptions.clone(),
        ..Default::default()
    });
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].group.name, "Calendars");
    assert_eq!(rows[0].status, "live");
    assert!(rows[0].notes.is_empty(), "{:?}", rows[0].notes);
    assert_eq!(
        rows[0].subscription,
        Some(SubscriptionMark {
            frequency: TickSubscriptionFrequency::High,
            is_waiting: false,
            next_check: None,
        }),
        "a quiet row waits for no check"
    );
    let period = TickSubscriptionFrequency::High.period().unwrap();
    tokio::time::advance(period).await;
    settle().await;
    assert_eq!(
        model.requests.lock().unwrap().len(),
        2,
        "baseline/quiet period must not trigger"
    );
    for sequence in 1..=3 {
        stream.push(TickObservation {
            instance: calendar(),
            sequence,
            observed_at: "2026-10-06T00:00:00Z".into(),
            dropped: 0,
            data: json!({"meeting":sequence}),
            kind: ObservationKind::Protocol,
            recover_with: Some("meetings".into()),
            gap: false,
        });
    }
    settle().await;
    let rows = monitor.list(&MonitorContext {
        subscriptions: agent.tick_subscriptions(chat).await.unwrap(),
        ..Default::default()
    });
    assert!(
        matches!(
            rows[0].subscription,
            Some(SubscriptionMark {
                frequency: TickSubscriptionFrequency::High,
                is_waiting: true,
                next_check: Some(NextCheck {
                    wait_ms: 60_000,
                    ..
                }),
            })
        ),
        "waiting updates fill toward the next check the Agent reports: {:?}",
        rows[0].subscription
    );
    tokio::time::advance(period).await;
    complete(&mut events).await;
    {
        let requests = model.requests.lock().unwrap();
        assert_eq!(requests.len(), 3);
        let messages = serde_json::to_string(&requests[2].messages).unwrap();
        assert!(
            messages.contains("cal:a") && messages.contains("\\\"meeting\\\":3"),
            "{messages}"
        );
    }
    agent.stop(chat).await.unwrap();
    settle().await;
    assert_eq!(
        stream.watched().len(),
        1,
        "a paused Loop keeps watching for the latest update"
    );
    agent.shutdown().await;
}

#[tokio::test]
async fn a_shell_subscription_reaches_the_loop_once_when_the_shell_exits() {
    let directory = tempfile::tempdir().unwrap();
    let model = Arc::new(ScriptedModel::new(vec![]));
    let agent = Agent::new_embedded(config(), model.clone());
    agent.install_own_tickhost();
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    let receipt = agent.submit_user_shell(chat,"printf baseline; while [ ! -f release ]; do sleep 0.02; done; printf ' final-output'; exit 7".into(),true).await.unwrap();
    let center = ShellTaskCenter::new(agent.clone());
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if center
                .snapshot(chat)
                .await
                .unwrap()
                .tasks
                .iter()
                .any(|task| task.id == receipt.command_id && task.output_bytes >= 8)
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    let adapters = Arc::new(
        TickAdapters::new([Arc::new(ShellAdapter::new(agent.clone())) as Arc<dyn TickAdapter>])
            .unwrap(),
    );
    agent.install_tick_stream(adapters).await;
    model.enqueue(call(
        "s1",
        "TickManage",
        json!({"action":"subscribe","tick_instance_id":InstanceRef::shell(receipt.command_id)}),
    ));
    model.enqueue(text("watching"));
    model.enqueue(text("shell completed"));
    let mut events = agent.subscribe();
    agent.submit(chat, "watch this shell".into()).await.unwrap();
    complete(&mut events).await;
    let subscription = agent.tick_subscriptions(chat).await.unwrap().remove(0);
    assert_eq!(subscription.frequency, TickSubscriptionFrequency::Immediate);
    tokio::time::sleep(SHELL_QUIET_WAIT).await;
    assert_eq!(
        model.requests.lock().unwrap().len(),
        2,
        "existing output is only a baseline"
    );
    std::fs::write(directory.path().join("release"), "").unwrap();
    let delivered = |needle: &str| {
        model
            .requests
            .lock()
            .unwrap()
            .iter()
            .skip(2)
            .any(|request| {
                serde_json::to_string(&request.messages)
                    .unwrap()
                    .contains(needle)
            })
    };
    tokio::time::timeout(Duration::from_secs(10), async {
        while !delivered("exit 7") {
            complete(&mut events).await;
        }
    })
    .await
    .expect("the exit is delivered at once, without a period");
    assert!(delivered("final-output"));
    assert_eq!(
        model.requests.lock().unwrap().len(),
        3,
        "the output before the exit is no update of its own"
    );
    let rows = MonitorRowsRegistry::builtin().list(&MonitorContext {
        shells: Some(center.snapshot(chat).await.unwrap()),
        subscriptions: agent.tick_subscriptions(chat).await.unwrap(),
        ..Default::default()
    });
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].group.name, "Shells");
    assert_eq!(rows[0].status, "exit 7");
    assert_eq!(rows[0].notes, ["7"]);
    assert_eq!(
        rows[0].subscription, None,
        "a Shell's immediate subscription shows no mark"
    );
    assert!(!rows[0].running);
    tokio::time::sleep(SHELL_QUIET_WAIT).await;
    assert_eq!(
        model.requests.lock().unwrap().len(),
        3,
        "finished tasks must become quiet"
    );
    agent
        .update_tick_subscription(chat, subscription.id, TickSubscriptionChange::Cancel)
        .await
        .unwrap();
    agent.shutdown().await;
}

/// A subscription never runs out of sight: its instance keeps a row even when no access lists
/// it or the user removed it, until the subscription itself is cancelled.
#[test]
fn a_subscribed_instance_always_has_a_row_with_its_marker() {
    let instance = InstanceRef::new("cal", "remote").unwrap();
    let mut chat: tickloop_agent_protocol::ChatSnapshot = serde_json::from_value(
        json!({"id":Uuid::new_v4(),"folder":"/fixture","messages":[],"chat_loop":{"id":"00000000-0000-0000-0000-000000000000","state":"Idle"}}),
    )
    .unwrap();
    chat.monitor.remove(instance.clone());
    let subscription = |accumulated: u64| -> tickloop_agent_protocol::TickSubscriptionStatus {
        serde_json::from_value(json!({"id":Uuid::new_v4(),"loop_id":Uuid::new_v4(),"loop_state":"Idle","instance":instance,"frequency":"high","accumulated":accumulated,"last_delivery_ms":null,"last_delivery":null,"updated_at_ms":5_000,"link":"Live"})).unwrap()
    };
    let unread = |rows: &[tickloop_agent_console::monitor_rows::TickDescriptor]| {
        rows[0]
            .update
            .as_ref()
            .and_then(|update| update.unread)
            .and_then(|unread| unread.count)
    };
    let listed = |subscriptions| {
        MonitorRowsRegistry::builtin().list(&MonitorContext {
            chat: Some(std::sync::Arc::new(
                tickloop_agent_protocol::ChatStatus::of(&chat),
            )),
            subscriptions,
            ..Default::default()
        })
    };
    assert!(listed(Vec::new()).is_empty(), "the removal hides the row");

    let rows = listed(vec![subscription(2)]);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].instance, instance);
    assert_eq!(rows[0].status, "live");
    let high = Some(SubscriptionMark {
        frequency: TickSubscriptionFrequency::High,
        is_waiting: true,
        next_check: None,
    });
    assert_eq!(rows[0].subscription, high);
    assert_eq!(unread(&rows), Some(2));

    let rows = listed(vec![subscription(0), subscription(1)]);
    assert_eq!(
        rows[0].subscription, high,
        "the mark never counts subscriptions"
    );
    assert!(rows[0].notes.is_empty(), "{:?}", rows[0].notes);
    assert_eq!(unread(&rows), Some(1));

    let mut interrupted = subscription(0);
    interrupted.loop_state = tickloop_agent_protocol::LoopState::Interrupted;
    assert_eq!(
        listed(vec![interrupted]).len(),
        1,
        "an interrupted turn leaves the subscription delivering"
    );
}

/// Built-in rows only describe what each key means; the Console decides how to carry it out.
#[test]
fn built_in_rows_describe_their_actions_as_intents() {
    use tickloop_agent_protocol::{
        MonitorMember, ShellTask, ShellTaskCapabilities, ShellTaskOrigin, ShellTaskOwner,
        ShellTaskSnapshot, ShellTaskState, TickSubscriptionStatus,
    };
    let chat = Uuid::new_v4();
    let shell = |state| ShellTask {
        id: Uuid::new_v4(),
        chat_id: chat,
        origin: ShellTaskOrigin::User,
        owner: ShellTaskOwner::User { chat_id: chat },
        call_id: None,
        command: "sleep 30".into(),
        description: None,
        cwd: "/fixture".into(),
        state,
        was_background: true,
        pid: None,
        exit_code: None,
        signal: None,
        reason: None,
        output_bytes: 0,
        updated_at_ms: None,
        ended_at_ms: None,
        capabilities: ShellTaskCapabilities::default(),
    };
    let (mut running, mut finished) = (
        shell(ShellTaskState::Background),
        shell(ShellTaskState::Completed),
    );
    running.capabilities.can_stop = true;
    let mut unstoppable = shell(ShellTaskState::Background);
    unstoppable.capabilities.can_stop = false;
    finished.updated_at_ms = Some(1_000);
    let subscription = |instance: &str| -> TickSubscriptionStatus {
        serde_json::from_value(json!({"id":Uuid::new_v4(),"loop_id":Uuid::new_v4(),"loop_state":"Idle","instance":instance,"frequency":"high","accumulated":0,"last_delivery_ms":null,"last_delivery":null,"link":"Live"})).unwrap()
    };
    let (pane, artifact) = (
        subscription("calendar:pane"),
        subscription("artifact:https://docs.example/a"),
    );
    let snapshot: tickloop_agent_protocol::ChatSnapshot = serde_json::from_value(json!({"id":chat,"folder":"/fixture","messages":[],"chat_loop":{"id":"00000000-0000-0000-0000-000000000000","state":"Idle"},"artifact_ticks":[{"artifact_id":"a","origin":"https://docs.example","title":"Brief","format":"markdown","url":"https://docs.example/a","access_role":"owner","added_at_ms":1}]})).unwrap();
    let rows = MonitorRowsRegistry::builtin().list(&MonitorContext {
        chat: Some(std::sync::Arc::new(
            tickloop_agent_protocol::ChatStatus::of(&snapshot),
        )),
        shells: Some(ShellTaskSnapshot {
            chat_id: chat,
            live_revision: 0,
            tasks: vec![running.clone(), unstoppable.clone(), finished.clone()],
        }),
        subscriptions: vec![pane.clone(), artifact.clone()],
        // The finished Shell still shows its result.
        now_ms: 1_000,
        ..Default::default()
    });
    let actions = |instance: &InstanceRef| {
        rows.iter()
            .find(|row| &row.instance == instance)
            .unwrap()
            .actions
            .clone()
    };
    let action = |key, label, intent| MonitorAction { key, label, intent };
    let running_instance = InstanceRef::shell(running.id);
    assert_eq!(
        actions(&running_instance),
        [
            action('o', "open", TickIntent::OpenView(running_instance.clone())),
            action('s', "stop", TickIntent::StopShell { task: running.id }),
            action(
                'x',
                "remove",
                TickIntent::Notify("Press s to stop this Shell before removing it".into()),
            ),
        ]
    );
    let unstoppable_instance = InstanceRef::shell(unstoppable.id);
    assert_eq!(
        actions(&unstoppable_instance),
        [
            action(
                'o',
                "open",
                TickIntent::OpenView(unstoppable_instance.clone())
            ),
            action(
                'x',
                "remove",
                TickIntent::Notify("This Shell leaves Monitor once it ends".into()),
            ),
        ],
        "a Shell this View may not stop offers no stop"
    );
    let finished_instance = InstanceRef::shell(finished.id);
    assert_eq!(
        actions(&finished_instance),
        [
            action('o', "open", TickIntent::OpenView(finished_instance.clone())),
            action(
                'x',
                "remove",
                TickIntent::RemoveFromMonitor(finished_instance.clone())
            ),
        ]
    );
    let pane_actions = vec![
        action('o', "open", TickIntent::ShowDetails(pane.instance.clone())),
        action(
            'x',
            "remove",
            TickIntent::RemoveFromMonitor(pane.instance.clone()),
        ),
        action(
            's',
            "subscriptions",
            TickIntent::Subscribe(pane.instance.clone()),
        ),
    ];
    assert_eq!(actions(&pane.instance), pane_actions);
    let artifact_actions = [
        ('o', "open"),
        ('x', "remove"),
        ('c', "copy"),
        ('f', "feedback"),
    ]
    .map(|(key, label)| {
        action(
            key,
            label,
            TickIntent::Artifact {
                id: artifact.instance.to_string(),
                key,
            },
        )
    })
    .to_vec();
    assert_eq!(
        actions(&artifact.instance),
        artifact_actions,
        "a Tick Subscription adds no keys to an artifact, whose association is its subscription"
    );
    let member = InstanceRef::new("cal", "member").unwrap();
    let member_row = tickloop_agent_console::monitor_rows::member_descriptor(&MonitorMember {
        is_terminal: false,
        instance: member.clone(),
        title: None,
    });
    assert_eq!(
        member_row.actions,
        [
            action('o', "details", TickIntent::ShowDetails(member.clone())),
            action('x', "remove", TickIntent::RemoveFromMonitor(member)),
        ]
    );
    assert_eq!(
        MonitorRowsRegistry::builtin().refresh_intents(),
        [TickIntent::RefreshArtifacts]
    );
}

#[test]
fn duplicate_instance_projection_keeps_the_newest_delivery_in_any_subscription_order() {
    use tickloop_agent_protocol::TickSubscriptionStatus;
    let row = |time: Option<u64>,
               title: Option<&str>,
               frequency: &str,
               link: &str|
     -> TickSubscriptionStatus {
        serde_json::from_value(json!({"id":Uuid::new_v4(),"loop_id":Uuid::new_v4(),"loop_state":"Idle","instance":"hosted:shared","frequency":frequency,"accumulated":0,"last_delivery_ms":time,"last_delivery":null,"kind":"Protocol","title":title,"updated_at_ms":time,"link":link})).unwrap()
    };
    let recent = row(Some(2000), Some("Shared Tick"), "high", "Live");
    let baseline = row(None, None, "low", "Initializing");
    for subscriptions in [
        vec![recent.clone(), baseline.clone()],
        vec![baseline.clone(), recent.clone()],
    ] {
        let rows = MonitorRowsRegistry::builtin().list(&MonitorContext {
            subscriptions,
            ..Default::default()
        });
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].updated_at_ms, Some(2000));
        assert_eq!(rows[0].title, "Shared Tick");
        assert_eq!(rows[0].status, "live");
        assert!(rows[0].notes.is_empty(), "{:?}", rows[0].notes);
        assert_eq!(
            rows[0].subscription,
            Some(SubscriptionMark {
                frequency: TickSubscriptionFrequency::High,
                is_waiting: false,
                next_check: None,
            }),
            "two subscriptions show as one mark in the most frequent one's colour"
        );
    }
}

#[tokio::test]
async fn shell_subscription_refuses_foreign_chat_before_baseline_and_restoration() {
    let directory = tempfile::tempdir().unwrap();
    let model = Arc::new(ScriptedModel::new(vec![]));
    let agent = Agent::new_embedded(config(), model.clone());
    let owner = agent.create_chat(directory.path().into()).await.unwrap();
    let observer = agent.create_chat(directory.path().into()).await.unwrap();
    let task = agent
        .submit_user_shell(
            owner,
            "printf baseline; while [ ! -f release ]; do sleep 0.02; done; printf foreign-secret"
                .into(),
            true,
        )
        .await
        .unwrap();
    let shell_stream = tickloop_agent_runtime::tick_adapter::ShellTickStream::new(agent.clone());
    let target = InstanceRef::shell(task.command_id);
    assert!(
        shell_stream
            .revalidate_watch(observer, &target, None)
            .await
            .is_err()
    );
    assert!(
        shell_stream
            .revalidate_watch(owner, &target, None)
            .await
            .is_ok()
    );
    assert!(
        shell_stream
            .revalidate_watch(
                owner,
                &target,
                Some(&tickloop_agent_protocol::HostToolAuthority::Mcp {
                    server_id: "https://unrelated.example".into(),
                    account_id: "unrelated".into()
                })
            )
            .await
            .is_err()
    );
    let adapters = Arc::new(TickAdapters::builtin(agent.clone(), None));
    let instance = InstanceRef::shell(task.command_id);
    agent.install_tick_stream(adapters).await;
    model.enqueue(call(
        "foreign-watch",
        "TickManage",
        json!({"action":"subscribe","tick_instance_id":instance}),
    ));
    model.enqueue(text("watch refused"));
    let mut events = agent.subscribe();
    agent
        .submit(observer, "watch the other Chat's shell".into())
        .await
        .unwrap();
    let mut refused = 0;
    loop {
        match next_event(&mut events).await.kind {
            EventKind::ToolCompleted { result, .. } => {
                assert!(result.is_error);
                refused += 1;
            }
            EventKind::Completed { .. } => break,
            _ => {}
        }
    }
    assert_eq!(refused, 1);
    assert!(
        agent.tick_subscriptions(observer).await.unwrap().is_empty(),
        "foreign Shell watch became configured"
    );
    agent
        .configure_trigger(
            observer,
            Some(tickloop_agent_protocol::Trigger {
                id: Uuid::new_v4(),
                prompt: "restored foreign watch".into(),
                prompt_source: tickloop_agent_protocol::TriggerPromptSource::Explicit,
                accepts_events: true,
                armed: true,
                subscriptions: vec![tickloop_agent_protocol::TickSubscription {
                    id: Uuid::new_v4(),
                    instance: instance.clone(),
                    frequency: tickloop_agent_protocol::TickSubscriptionFrequency::Immediate,
                    authority: None,
                }],
            }),
        )
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let rows = agent.tick_subscriptions(observer).await.unwrap();
            if rows[0].link == tickloop_agent_protocol::TickLinkState::IdentityUncertain {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("restored foreign watch must fail closed before its baseline");
    std::fs::write(directory.path().join("release"), "").unwrap();
    tokio::time::sleep(SHELL_QUIET_WAIT).await;
    assert!(!model.requests.lock().unwrap().iter().any(|request| {
        serde_json::to_string(&request.messages)
            .unwrap()
            .contains("foreign-secret")
    }));
    agent.shutdown().await;
}

#[tokio::test]
async fn an_artifact_row_prefers_its_cached_title_over_the_joined_one() {
    let directory = tempfile::tempdir().unwrap();
    let agent = Agent::new_embedded(config(), Arc::new(ScriptedModel::new(vec![])));
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    let tick = tickloop_agent_protocol::ArtifactTick {
        artifact_id: "doc".into(),
        origin: "https://docs.example".into(),
        title: "Joined title".into(),
        format: "markdown".into(),
        url: "https://docs.example/a/doc".into(),
        access_role: "owner".into(),
        added_at_ms: 1,
    };
    agent.add_artifact_tick(chat, tick.clone()).await.unwrap();
    let mut context = MonitorContext {
        chat: Some(std::sync::Arc::new(agent.status(chat).await.unwrap())),
        ..Default::default()
    };
    let registry = MonitorRowsRegistry::builtin();
    assert_eq!(registry.list(&context)[0].title, "Joined title");
    context.metadata.insert(tick.instance_id(),Some(serde_json::from_value(json!({"id":"doc","title":"Cached title","format":"markdown","access_role":"owner"})).unwrap()));
    context
        .metadata_failures
        .insert(tick.instance_id(), "refresh failed".into());
    assert_eq!(registry.list(&context)[0].title, "Cached title");
    agent.shutdown().await;
}

#[tokio::test]
async fn artifact_rows_show_where_feedback_is_linked_and_the_newest_request() {
    let directory = tempfile::tempdir().unwrap();
    let agent = Agent::new_embedded(config(), Arc::new(ScriptedModel::new(vec![])));
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    let tick = tickloop_agent_protocol::ArtifactTick {
        artifact_id: "doc".into(),
        origin: "https://docs.example".into(),
        title: "Linked".into(),
        format: "markdown".into(),
        url: "https://docs.example/a/doc".into(),
        access_role: "owner".into(),
        added_at_ms: 1,
    };
    agent.add_artifact_tick(chat, tick.clone()).await.unwrap();
    let here = Uuid::new_v4();
    let elsewhere = Uuid::new_v4();
    let base = agent.snapshot(chat).await.unwrap();
    let row = |paused: Option<bool>, remote: Option<Uuid>| {
        let mut snapshot = base.clone();
        if let Some(resume_required) = paused {
            snapshot.chat_loop.event_sources = serde_json::from_value(json!([{"origin":"https://docs.example","account_id":"owner","artifact_id":"doc","binding_id":here,"client_instance_id":Uuid::new_v4(),"epoch":2,"resume_required":resume_required,"last_request_at_ms":1_791_453_600_000_u64}])).unwrap();
        }
        let mut context = MonitorContext {
            chat: Some(std::sync::Arc::new(
                tickloop_agent_protocol::ChatStatus::of(&snapshot),
            )),
            ..Default::default()
        };
        context.metadata.insert(tick.instance_id(), Some(serde_json::from_value(json!({"id":"doc","title":"Linked","format":"markdown","access_role":"owner","human_updated_at":"2026-10-08T09:00:00.000Z","agent_links":remote.map(|binding_id| vec![json!({"binding_id":binding_id,"chat_id":Uuid::new_v4(),"loop_id":Uuid::new_v4(),"epoch":2})]).unwrap_or_default()})).unwrap()));
        MonitorRowsRegistry::builtin().list(&context).remove(0)
    };
    assert_eq!(row(None, None).status, "");
    assert!(row(None, None).notes.is_empty());
    assert_eq!(row(None, Some(elsewhere)).status, "feedback elsewhere");
    assert_eq!(
        row(None, Some(elsewhere)).notes,
        ["feedback elsewhere"],
        "where feedback goes stays on the row"
    );
    assert_eq!(row(Some(false), Some(elsewhere)).status, "feedback here");
    assert!(
        row(Some(false), Some(here)).notes.is_empty(),
        "an association here is the row's default subscription and needs no words"
    );
    assert_eq!(row(Some(false), Some(here)).subscription, None);
    assert_eq!(row(Some(true), Some(here)).status, "feedback unavailable");
    assert_eq!(row(Some(true), Some(here)).notes, ["feedback unavailable"]);
    assert_eq!(row(Some(true), Some(here)).subscription, None);
    let x_label = |paused, remote| {
        row(paused, remote)
            .actions
            .into_iter()
            .find(|action| action.key == 'x')
            .unwrap()
            .label
    };
    assert_eq!(x_label(Some(false), Some(here)), "unlink");
    assert_eq!(x_label(Some(true), Some(here)), "unlink");
    assert_eq!(
        x_label(None, Some(elsewhere)),
        "remove",
        "x leaves another Chat's association alone"
    );
    assert_eq!(
        row(Some(true), Some(elsewhere)).status,
        "feedback elsewhere"
    );
    assert_eq!(
        row(Some(false), Some(here)).updated_at_ms,
        Some(1_791_453_600_000),
        "a delivered request is the newest update"
    );
    assert_eq!(row(None, None).updated_at_ms, Some(1_791_450_000_000));
    agent.shutdown().await;
}
