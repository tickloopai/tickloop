use serde_json::json;
use std::{sync::Arc, time::Duration};
use tick_protocol::InstanceRef;
use tickloop_agent_console::monitor_rows::{
    MonitorAction, MonitorContext, MonitorRows, MonitorRowsRegistry, TickDescriptor, TickGlyph,
    TickGroup, TickIntent,
};
use tickloop_agent_protocol::{EventKind, ObservationKind, TickSubscriptionChange};
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
    fn tick_type(&self) -> &'static str {
        "cal"
    }
    fn stream(&self) -> Option<Arc<dyn TickStreamPort>> {
        Some(Arc::new(self.stream.clone()))
    }
}
struct CalendarRows;
impl MonitorRows for CalendarRows {
    fn tick_type(&self) -> &'static str {
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
            owner: tickloop_agent_console::monitor_rows::TickOwner::Unknown,
            updated_at_ms: None,
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
            "SubscribeTick",
            json!({"tick_instance_id":calendar(),"every_seconds":5}),
        ),
        text("watching"),
        text("calendar changed"),
    ]));
    let agent = Agent::new_embedded(config(), model.clone());
    agent.install_tick_stream(adapters).await;
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    let mut events = agent.subscribe();
    let receipt = agent
        .submit(chat, None, "watch the earnings calendar".into())
        .await
        .unwrap();
    complete(&mut events).await;
    {
        let requests = model.requests.lock().unwrap();
        let subscribe = requests[0]
            .tools
            .iter()
            .find(|tool| tool.name == "SubscribeTick")
            .unwrap();
        assert!(
            jsonschema::validator_for(&subscribe.input_schema)
                .unwrap()
                .is_valid(&json!({"tick_instance_id":"cal:a","every_seconds":5})),
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
    assert_eq!(rows[0].status, "live · 5s");
    tokio::time::advance(Duration::from_secs(5)).await;
    settle().await;
    assert_eq!(
        model.requests.lock().unwrap().len(),
        2,
        "baseline/quiet cadence must not trigger"
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
    tokio::time::advance(Duration::from_secs(5)).await;
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
    agent.stop(chat, receipt.loop_id).await.unwrap();
    settle().await;
    assert!(stream.watched().is_empty());
    agent.shutdown().await;
}

#[tokio::test]
async fn shell_baseline_output_and_final_exit_use_the_same_subscription_to_loop_path() {
    let directory = tempfile::tempdir().unwrap();
    let model = Arc::new(ScriptedModel::new(vec![]));
    let agent = Agent::new_embedded(config(), model.clone());
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    let receipt = agent.submit_user_shell_for_goal(chat,None,"printf baseline; while [ ! -f release ]; do sleep 0.02; done; printf ' final-output'; exit 7".into(),true).await.unwrap();
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
        "SubscribeTick",
        json!({"tick_instance_id":InstanceRef::shell(receipt.command_id),"every_seconds":5}),
    ));
    model.enqueue(text("watching"));
    model.enqueue(text("shell completed"));
    let mut events = agent.subscribe();
    agent
        .submit(chat, None, "watch this shell".into())
        .await
        .unwrap();
    complete(&mut events).await;
    let subscription = agent.tick_subscriptions(chat).await.unwrap().remove(0);
    tokio::time::sleep(Duration::from_millis(5200)).await;
    assert_eq!(
        model.requests.lock().unwrap().len(),
        2,
        "existing output is only a baseline"
    );
    std::fs::write(directory.path().join("release"), "").unwrap();
    complete(&mut events).await;
    {
        let requests = model.requests.lock().unwrap();
        assert_eq!(requests.len(), 3);
        let messages = serde_json::to_string(&requests[2].messages).unwrap();
        assert!(
            messages.contains("final-output") && messages.contains("exit 7"),
            "{messages}"
        );
    }
    let rows = MonitorRowsRegistry::builtin().list(&MonitorContext {
        shells: Some(center.snapshot(chat).await.unwrap()),
        subscriptions: agent.tick_subscriptions(chat).await.unwrap(),
        ..Default::default()
    });
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].group.name, "Shells");
    assert_eq!(rows[0].status, "exit 7 · 5s");
    assert!(!rows[0].running);
    tokio::time::sleep(Duration::from_millis(5200)).await;
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

#[tokio::test]
async fn shell_output_changes_before_exit_and_a_late_watch_uses_a_new_baseline() {
    use tickloop_agent_runtime::tick_stream::TickStreamEvent;
    let directory = tempfile::tempdir().unwrap();
    let agent = Agent::new_embedded(config(), Arc::new(ScriptedModel::new(vec![])));
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    let task = agent.submit_user_shell_for_goal(chat,None,"printf baseline; while [ ! -f output ]; do sleep 0.02; done; printf ' live-output'; while [ ! -f finish ]; do sleep 0.02; done; exit 7".into(),true).await.unwrap();
    let center = ShellTaskCenter::new(agent.clone());
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if center
                .snapshot(chat)
                .await
                .unwrap()
                .tasks
                .iter()
                .any(|row| row.id == task.command_id && row.output_bytes == 8)
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    let adapters = TickAdapters::builtin(agent.clone(), None, None, None);
    let mut stream = adapters.open();
    let instance = InstanceRef::shell(task.command_id);
    let first = Uuid::new_v4();
    adapters.set_watch_chats(std::collections::BTreeMap::from([(first, chat)]));
    stream
        .desired
        .send_replace(std::collections::BTreeMap::from([(
            first,
            instance.clone(),
        )]));
    tokio::time::timeout(Duration::from_secs(5),async { loop { if matches!(stream.events.recv().await,Some(TickStreamEvent::Ready { watch }) if watch == first) { break } } }).await.unwrap();
    std::fs::write(directory.path().join("output"), "").unwrap();
    let live = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Some(TickStreamEvent::Observation { value, .. }) = stream.events.recv().await {
                break value;
            }
        }
    })
    .await
    .unwrap();
    assert_eq!(live.data["status"], "running");
    assert_eq!(live.data["output"], "baseline live-output");
    std::fs::write(directory.path().join("finish"), "").unwrap();
    let final_update = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Some(TickStreamEvent::Observation { value, .. }) = stream.events.recv().await {
                break value;
            }
        }
    })
    .await
    .unwrap();
    assert_eq!(final_update.data["exit_code"], 7);
    assert!(final_update.sequence > live.sequence);
    let late = Uuid::new_v4();
    adapters.set_watch_chats(std::collections::BTreeMap::from([
        (first, chat),
        (late, chat),
    ]));
    stream
        .desired
        .send_replace(std::collections::BTreeMap::from([
            (first, instance.clone()),
            (late, instance),
        ]));
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match stream.events.recv().await {
                Some(TickStreamEvent::Ready { watch }) if watch == late => break,
                Some(TickStreamEvent::Observation { .. }) => {
                    panic!("late subscription received an old result")
                }
                _ => {}
            }
        }
    })
    .await
    .unwrap();
    assert!(
        tokio::time::timeout(Duration::from_millis(600), async {
            loop {
                if matches!(
                    stream.events.recv().await,
                    Some(TickStreamEvent::Observation { .. })
                ) {
                    break;
                }
            }
        })
        .await
        .is_err()
    );
    agent.shutdown().await;
}

/// Built-in rows only describe what each key means; the Console decides how to carry it out.
#[test]
fn built_in_rows_describe_their_actions_as_intents() {
    use tickloop_agent_protocol::{
        MonitorMember, MonitorMemberSource, ShellTask, ShellTaskCapabilities, ShellTaskOrigin,
        ShellTaskOwner, ShellTaskSnapshot, ShellTaskState, TickSubscriptionStatus,
    };
    let chat = Uuid::new_v4();
    let shell = |state| ShellTask {
        id: Uuid::new_v4(),
        chat_id: chat,
        origin: ShellTaskOrigin::User,
        owner: ShellTaskOwner::User { chat_id: chat },
        command: "sleep 30".into(),
        cwd: "/fixture".into(),
        state,
        was_background: true,
        pid: None,
        exit_code: None,
        signal: None,
        reason: None,
        output_bytes: 0,
        updated_at_ms: None,
        capabilities: ShellTaskCapabilities::default(),
    };
    let (running, finished) = (
        shell(ShellTaskState::Background),
        shell(ShellTaskState::Completed),
    );
    let subscription = |instance: &str| -> TickSubscriptionStatus {
        serde_json::from_value(json!({"id":Uuid::new_v4(),"loop_id":Uuid::new_v4(),"loop_state":"WaitingTrigger","instance":instance,"every_seconds":60,"accumulated":0,"last_delivery_ms":null,"last_delivery":null,"link":"Live"})).unwrap()
    };
    let (pane, artifact) = (
        subscription("herdr:pane"),
        subscription("artifact:https://docs.example/a"),
    );
    let rows = MonitorRowsRegistry::builtin().list(&MonitorContext {
        shells: Some(ShellTaskSnapshot {
            chat_id: chat,
            live_revision: 0,
            tasks: vec![running.clone(), finished.clone()],
        }),
        subscriptions: vec![pane.clone(), artifact.clone()],
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
    assert_eq!(
        actions(&InstanceRef::shell(running.id)),
        [
            action('o', "open", TickIntent::OpenShell { task: running.id }),
            action(
                'x',
                "remove",
                TickIntent::Notify("Open this Shell to stop it before removing it".into())
            ),
        ]
    );
    let finished_instance = InstanceRef::shell(finished.id);
    assert_eq!(
        actions(&finished_instance),
        [
            action('o', "open", TickIntent::OpenShell { task: finished.id }),
            action(
                'x',
                "remove",
                TickIntent::RemoveFromMonitor(finished_instance.clone())
            ),
        ]
    );
    assert_eq!(
        actions(&pane.instance),
        [
            action('o', "open", TickIntent::OpenSubscription { id: pane.id }),
            action(
                'x',
                "remove",
                TickIntent::RemoveFromMonitor(pane.instance.clone())
            ),
        ]
    );
    assert_eq!(
        actions(&artifact.instance),
        [
            ('o', "open"),
            ('x', "remove"),
            ('c', "copy"),
            ('f', "feedback")
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
    );
    let member = InstanceRef::new("cal", "member").unwrap();
    let member_row = tickloop_agent_console::monitor_rows::member_descriptor(&MonitorMember {
        instance: member.clone(),
        source: MonitorMemberSource::Added,
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
               cadence: u64,
               link: &str|
     -> TickSubscriptionStatus {
        serde_json::from_value(json!({"id":Uuid::new_v4(),"loop_id":Uuid::new_v4(),"loop_state":"WaitingTrigger","instance":"herdr:shared","every_seconds":cadence,"accumulated":0,"last_delivery_ms":time,"last_delivery":null,"kind":"Protocol","title":title,"updated_at_ms":time,"link":link})).unwrap()
    };
    let recent = row(Some(2000), Some("Shared Tick"), 5, "Live");
    let baseline = row(None, None, 86400, "Initializing");
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
        assert!(rows[0].status.starts_with("live · "));
        assert!(rows[0].status.contains("5s") && rows[0].status.contains("86400s"));
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
        .submit_user_shell_for_goal(
            owner,
            None,
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
    let adapters = Arc::new(TickAdapters::builtin(agent.clone(), None, None, None));
    let instance = InstanceRef::shell(task.command_id);
    agent.install_tick_stream(adapters).await;
    model.enqueue(call(
        "foreign-watch",
        "SubscribeTick",
        json!({"tick_instance_id":instance,"every_seconds":5}),
    ));
    model.enqueue(text("watch refused"));
    let mut events = agent.subscribe();
    let receipt = agent
        .submit(observer, None, "watch the other Chat's shell".into())
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
            receipt.loop_id,
            Some(tickloop_agent_protocol::Trigger {
                id: Uuid::new_v4(),
                prompt: "restored foreign watch".into(),
                interval_seconds: None,
                expires_at_ms: None,
                accepts_events: true,
                armed: true,
                subscriptions: vec![tickloop_agent_protocol::TickSubscription {
                    id: Uuid::new_v4(),
                    instance: instance.clone(),
                    every_seconds: 5,
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
    tokio::time::sleep(Duration::from_millis(5200)).await;
    assert!(!model.requests.lock().unwrap().iter().any(|request| {
        serde_json::to_string(&request.messages)
            .unwrap()
            .contains("foreign-secret")
    }));
    agent.shutdown().await;
}

#[tokio::test]
async fn a_titleless_artifact_subscription_preserves_joined_and_cached_titles() {
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
    let row:tickloop_agent_protocol::TickSubscriptionStatus=serde_json::from_value(json!({"id":Uuid::new_v4(),"loop_id":Uuid::new_v4(),"loop_state":"WaitingTrigger","instance":tick.instance_id(),"every_seconds":60,"accumulated":0,"last_delivery_ms":null,"last_delivery":null,"kind":"Protocol","title":null,"updated_at_ms":null,"link":"Disconnected"})).unwrap();
    let mut context = MonitorContext {
        chat: Some(agent.snapshot(chat).await.unwrap()),
        subscriptions: vec![row],
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
