use std::sync::Arc;
use tickloop_agent_settings::local_agent::LocalAgent;

use async_trait::async_trait;
use serde_json::{Value, json};
use tick_protocol::InstanceRef;
use tickloop_agent_console::monitor_rows::{
    MonitorAction, MonitorContext, MonitorRows, MonitorRowsRegistry, TickDescriptor, TickGlyph,
    TickGroup, TickIntent, TickOwner,
};
use tickloop_agent_protocol::{
    ChatId, EventKind, TickSubscriptionChange, ToolDefinition, ToolResult,
};
use tickloop_agent_runtime::{
    Agent,
    host_tools::{
        HostCallResult, HostEffect, HostToolPermission, HostToolProvider, PreparedHostCall,
    },
    tick_adapter::{TickAdapter, TickAdapters},
    tick_stream::{InMemoryTickStream, TickStreamPort},
};
use tokio_util::sync::CancellationToken;

const OPEN_TOOL: &str = "open_calendar";

fn calendar(id: &str) -> InstanceRef {
    InstanceRef::new("cal", id).unwrap()
}

/// Opens one controlled Tick and reports only the verified instance it created.
struct OpeningHost;

#[async_trait]
impl HostToolProvider for OpeningHost {
    fn definitions(&self) -> Vec<ToolDefinition> {
        vec![ToolDefinition {
            name: OPEN_TOOL.into(),
            description: "Open the calendar Tick".into(),
            input_schema: json!({"type":"object"}),
        }]
    }
    fn permission(&self, _: ChatId, _: &str, _: &Value) -> HostToolPermission {
        HostToolPermission::Automatic
    }
    async fn execute(&self, _: ChatId, _: &str, _: Value, _: CancellationToken) -> ToolResult {
        unreachable!("prepared effect seam")
    }
    async fn execute_prepared(
        &self,
        _: ChatId,
        _: PreparedHostCall,
        _: CancellationToken,
    ) -> HostCallResult {
        HostCallResult::CompletedWithEffect(
            ToolResult {
                content: json!({"tick_instance_id":"cal:opened","status":"running"}).to_string(),
                is_error: false,
                output_ref: None,
            },
            Box::new(HostEffect::OpenedTick {
                is_terminal: false,
                instance: calendar("opened"),
                title: Some("Earnings calendar".into()),
            }),
        )
    }
}

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

/// Lists every member and subscription of its type through the shared Monitor contract.
struct CalendarRows;
impl MonitorRows for CalendarRows {
    fn access(&self) -> &'static str {
        "cal"
    }
    fn list(&self, context: &MonitorContext) -> Vec<TickDescriptor> {
        context
            .subscriptions
            .iter()
            .filter(|row| row.instance.access() == "cal")
            .map(|row| TickDescriptor {
                instance: row.instance.clone(),
                group: TickGroup::Ticks,
                glyph: TickGlyph::Protocol,
                title: row.instance.to_string(),
                kind: "Calendar".into(),
                status: "live".into(),
                notes: Vec::new(),
                subscription: None,
                owner: TickOwner::Unknown,
                tone: tickloop_agent_console::monitor_rows::StatusTone::Neutral,
                updated_at_ms: row.last_delivery_ms,
                update: None,
                running: true,
                resources: None,
                actions: vec![MonitorAction {
                    key: 'o',
                    label: "open",
                    intent: TickIntent::Notify("opened".into()),
                }],
            })
            .collect()
    }
}

async fn run_turn(agent: &Agent, chat: ChatId, text: &str) {
    let mut events = agent.subscribe();
    agent.submit(chat, text.into()).await.unwrap();
    loop {
        match tickloop_agent_test_support::next_event(&mut events)
            .await
            .kind
        {
            EventKind::Completed { .. } => break,
            EventKind::Failed(error) => panic!("{error}"),
            _ => {}
        }
    }
}

fn adapters() -> Arc<TickAdapters> {
    Arc::new(
        TickAdapters::new([Arc::new(CalendarAdapter {
            stream: InMemoryTickStream::new(),
        }) as Arc<dyn TickAdapter>])
        .unwrap(),
    )
}

fn monitor() -> MonitorRowsRegistry {
    MonitorRowsRegistry::new([Arc::new(CalendarRows) as Arc<dyn MonitorRows>]).unwrap()
}

async fn monitor_rows(agent: &Agent, chat: ChatId) -> Vec<TickDescriptor> {
    monitor().list(&MonitorContext {
        chat: Some(std::sync::Arc::new(agent.status(chat).await.unwrap())),
        subscriptions: agent.tick_subscriptions(chat).await.unwrap(),
        ..Default::default()
    })
}

#[tokio::test]
async fn an_unsubscribed_open_joins_only_its_chat_monitor_without_watch_or_model_work() {
    let directory = tempfile::tempdir().unwrap();
    let model = Arc::new(tickloop_agent_test_support::ScriptedModel::new(vec![
        tickloop_agent_test_support::call("open", OPEN_TOOL, json!({})),
        tickloop_agent_test_support::text("opened"),
    ]));
    let agent = Agent::new_embedded(tickloop_agent_test_support::config(), model.clone());
    agent.install_host_tools(Arc::new(OpeningHost)).unwrap();
    agent.install_tick_stream(adapters()).await;
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    let other = agent.create_chat(directory.path().into()).await.unwrap();
    run_turn(&agent, chat, "open the calendar").await;

    let snapshot = agent.snapshot(chat).await.unwrap();
    assert_eq!(snapshot.monitor.members.len(), 1);
    assert_eq!(snapshot.monitor.members[0].instance, calendar("opened"));
    let rows = monitor_rows(&agent, chat).await;
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0].instance, calendar("opened"));
    assert_eq!(rows[0].title, "Earnings calendar");
    assert_eq!(rows[0].owner, TickOwner::ThisChat);
    assert!(rows[0].resources.is_none(), "resources are not invented");
    assert!(agent.tick_subscriptions(chat).await.unwrap().is_empty());
    assert!(monitor_rows(&agent, other).await.is_empty());

    let requests = model.requests.lock().unwrap().len();
    for _ in 0..3 {
        monitor_rows(&agent, chat).await;
    }
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    assert_eq!(
        model.requests.lock().unwrap().len(),
        requests,
        "listing never calls the model"
    );
    assert!(agent.tick_subscriptions(chat).await.unwrap().is_empty());

    agent
        .remove_monitor_member(chat, calendar("hidden"))
        .await
        .unwrap();
    let branch = agent
        .branch_chat(tickloop_agent_protocol::BranchRequest {
            request_id: uuid::Uuid::new_v4(),
            source_chat_id: chat,
            name: Some("Branch".into()),
        })
        .await
        .unwrap();
    let branched = agent.snapshot(branch.child_chat_id).await.unwrap().monitor;
    assert!(
        branched.members.is_empty(),
        "a Branch is a new Chat and inherits no opened Tick"
    );
    assert!(
        branched.is_removed(&calendar("hidden")),
        "removals carry over so copied subscriptions stay hidden"
    );
    assert!(monitor_rows(&agent, branch.child_chat_id).await.is_empty());
    agent.shutdown().await;
}

#[tokio::test]
async fn removal_outlives_restart_but_never_hides_a_subscription() {
    let directory = tempfile::tempdir().unwrap();
    let store = directory.path().join("archive");
    let model = Arc::new(tickloop_agent_test_support::ScriptedModel::new(vec![
        tickloop_agent_test_support::call(
            "subscribe",
            "TickManage",
            json!({"action":"subscribe","tick_instance_id":"cal:watched","frequency":"low"}),
        ),
        tickloop_agent_test_support::text("watching"),
    ]));
    let agent =
        Agent::open_embedded(store.clone(), tickloop_agent_test_support::config(), model).unwrap();
    agent.install_tick_stream(adapters()).await;
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    run_turn(&agent, chat, "watch the calendar").await;
    assert_eq!(monitor_rows(&agent, chat).await.len(), 1);

    agent
        .remove_monitor_member(chat, calendar("watched"))
        .await
        .unwrap();
    let subscriptions = agent.tick_subscriptions(chat).await.unwrap();
    assert_eq!(
        subscriptions.len(),
        1,
        "removal leaves the subscription in place"
    );
    assert_eq!(
        monitor_rows(&agent, chat).await.len(),
        1,
        "a subscription keeps its row in sight"
    );
    agent
        .update_tick_subscription(chat, subscriptions[0].id, TickSubscriptionChange::Cancel)
        .await
        .unwrap();
    assert!(monitor_rows(&agent, chat).await.is_empty());
    agent.shutdown().await;
    drop(agent);

    let restored = Agent::open_embedded(
        store,
        tickloop_agent_test_support::config(),
        Arc::new(tickloop_agent_test_support::ScriptedModel::new(Vec::new())),
    )
    .unwrap();
    restored.install_tick_stream(adapters()).await;
    restored.resume_chat(chat).await.unwrap();
    assert!(
        monitor_rows(&restored, chat).await.is_empty(),
        "restart and projection keep the removal"
    );
    restored.shutdown().await;
}

#[tokio::test]
async fn an_empty_chat_keeps_members_and_removals_across_restart() {
    let directory = tempfile::tempdir().unwrap();
    let store = directory.path().join("archive");
    let agent = Agent::open_embedded(
        store.clone(),
        tickloop_agent_test_support::config(),
        Arc::new(tickloop_agent_test_support::ScriptedModel::new(Vec::new())),
    )
    .unwrap();
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    agent
        .record_reopened_tick(chat, &calendar("lost"), calendar("kept"))
        .await
        .unwrap();
    agent
        .remove_monitor_member(chat, calendar("hidden"))
        .await
        .unwrap();
    agent.shutdown().await;
    drop(agent);

    let restored = Agent::open_embedded(
        store,
        tickloop_agent_test_support::config(),
        Arc::new(tickloop_agent_test_support::ScriptedModel::new(Vec::new())),
    )
    .unwrap();
    restored.resume_chat(chat).await.unwrap();
    let membership = restored.snapshot(chat).await.unwrap().monitor;
    assert_eq!(membership.members.len(), 1);
    assert_eq!(membership.members[0].instance, calendar("kept"));
    assert!(membership.removed.contains(&calendar("hidden")));
    restored.shutdown().await;
}

#[tokio::test(start_paused = true)]
async fn one_row_merges_every_source_and_frequency_and_pages_cover_the_full_set() {
    let directory = tempfile::tempdir().unwrap();
    let model = Arc::new(tickloop_agent_test_support::ScriptedModel::new(
        ["high", "medium"]
            .into_iter()
            .flat_map(|frequency| {
                vec![
                    tickloop_agent_test_support::call(
                        &format!("s-{frequency}"),
                        "TickManage",
                        json!({"action":"subscribe","tick_instance_id":"cal:shared","frequency":frequency}),
                    ),
                    tickloop_agent_test_support::text("watching"),
                ]
            })
            .collect(),
    ));
    let agent = Agent::new_embedded(tickloop_agent_test_support::config(), model.clone());
    agent.install_tick_stream(adapters()).await;
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    run_turn(&agent, chat, "first watcher").await;
    run_turn(&agent, chat, "second watcher").await;
    let requests = model.requests.lock().unwrap().len();
    let mut snapshot = agent.snapshot(chat).await.unwrap();
    snapshot
        .monitor
        .record_opened(calendar("shared"), Some("Shared calendar".into()));
    for index in 0..200 {
        snapshot
            .monitor
            .record_opened(calendar(&format!("m{index:03}")), None);
    }
    let subscriptions = agent.tick_subscriptions(chat).await.unwrap();
    let context = MonitorContext {
        chat: Some(std::sync::Arc::new(
            tickloop_agent_protocol::ChatStatus::of(&snapshot),
        )),
        subscriptions: subscriptions.clone(),
        ..Default::default()
    };
    let monitor = monitor();
    let rows = monitor.list(&context);
    assert_eq!(
        rows.len(),
        201,
        "Monitor N counts the deduplicated full set"
    );
    let shared = rows
        .iter()
        .find(|row| row.instance == calendar("shared"))
        .unwrap();
    assert_eq!(shared.title, "Shared calendar");
    assert!(shared.notes.is_empty(), "{:?}", shared.notes);
    assert!(
        shared.subscription.is_some(),
        "both subscriptions show as one mark: {:?}",
        shared.subscription
    );

    let mut seen = Vec::new();
    let mut cursor = None;
    loop {
        let page = monitor.page(&context, cursor.as_ref(), None).unwrap();
        assert_eq!(page.total, 201);
        assert!(page.items.len() <= 128);
        seen.extend(page.items.into_iter().map(|row| row.instance));
        match page.next {
            Some(next) => cursor = Some(next),
            None => break,
        }
    }
    let mut expected = rows
        .iter()
        .map(|row| row.instance.clone())
        .collect::<Vec<_>>();
    expected.sort();
    seen.sort();
    assert_eq!(seen, expected);

    let first = monitor.page(&context, None, None).unwrap();
    snapshot.monitor.remove(calendar("m000"));
    let changed = MonitorContext {
        chat: Some(std::sync::Arc::new(
            tickloop_agent_protocol::ChatStatus::of(&snapshot),
        )),
        subscriptions,
        ..Default::default()
    };
    assert!(
        monitor.page(&changed, first.next.as_ref(), None).is_err(),
        "a changed set expires the old cursor"
    );
    assert_eq!(model.requests.lock().unwrap().len(), requests);
    agent.shutdown().await;
}

#[tokio::test]
async fn rewind_and_compaction_keep_membership_and_restart_reads_it_back() {
    let directory = tempfile::tempdir().unwrap();
    let store = directory.path().join("archive");
    let model = Arc::new(tickloop_agent_test_support::ScriptedModel::new(vec![
        tickloop_agent_test_support::call("open", OPEN_TOOL, json!({})),
        tickloop_agent_test_support::text("opened"),
        tickloop_agent_test_support::text("second reply"),
        tickloop_agent_test_support::text("summary of the conversation"),
        tickloop_agent_test_support::text("third reply"),
    ]));
    let agent =
        Agent::open_embedded(store.clone(), tickloop_agent_test_support::config(), model).unwrap();
    agent.install_host_tools(Arc::new(OpeningHost)).unwrap();
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    run_turn(&agent, chat, "open the calendar").await;
    run_turn(&agent, chat, "say more").await;
    agent
        .remove_monitor_member(chat, calendar("elsewhere"))
        .await
        .unwrap();
    let before = agent.snapshot(chat).await.unwrap().monitor;
    assert_eq!(before.members.len(), 1);

    agent.compact(chat).await.unwrap();
    assert_eq!(agent.snapshot(chat).await.unwrap().monitor, before);
    run_turn(&agent, chat, "after compaction").await;
    let point = agent
        .snapshot(chat)
        .await
        .unwrap()
        .rewind_points()
        .last()
        .unwrap()
        .clone();
    agent
        .rewind(
            chat,
            point.message_id,
            tickloop_agent_protocol::RewindMode::Conversation,
        )
        .await
        .unwrap();
    assert_eq!(
        agent.snapshot(chat).await.unwrap().monitor,
        before,
        "rewinding the conversation neither forgets the opened Tick nor its removals"
    );
    agent.shutdown().await;
    drop(agent);

    let restored = Agent::open_embedded(
        store,
        tickloop_agent_test_support::config(),
        Arc::new(tickloop_agent_test_support::ScriptedModel::new(Vec::new())),
    )
    .unwrap();
    restored.resume_chat(chat).await.unwrap();
    assert_eq!(restored.snapshot(chat).await.unwrap().monitor, before);
    restored.shutdown().await;
}

/// A hosted row shows what the latest probe found, running with its process sample, exited,
/// gone or lost with its host, and its one `x` and Enter follow from that and from whether this
/// Chat opened the Tick.
#[tokio::test]
async fn hosted_rows_offer_one_x_and_enter_by_what_still_runs_and_who_opened_it() {
    use std::collections::HashMap;
    use tickloop_agent_console::monitor_rows::{MemberState, TickResources};
    let directory = tempfile::tempdir().unwrap();
    let agent = Agent::new_embedded(
        tickloop_agent_test_support::config(),
        Arc::new(tickloop_agent_test_support::ScriptedModel::new(Vec::new())),
    );
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    let (running, exited, gone, lost, foreign) = (
        InstanceRef::hosted(uuid::Uuid::new_v4()),
        InstanceRef::hosted(uuid::Uuid::new_v4()),
        InstanceRef::hosted(uuid::Uuid::new_v4()),
        InstanceRef::hosted(uuid::Uuid::new_v4()),
        InstanceRef::hosted(uuid::Uuid::new_v4()),
    );
    let mut snapshot = agent.snapshot(chat).await.unwrap();
    for instance in [&running, &exited, &gone, &lost] {
        snapshot
            .monitor
            .record_opened(instance.clone(), Some("Counter".into()));
    }
    let sample = TickResources {
        pid: 4242,
        cpu_time_ms: Some(120),
        cpu_percent: None,
        rss_bytes: Some(8 << 20),
        sampled_at_ms: 1,
    };
    let only_subscribed: tickloop_agent_protocol::TickSubscriptionStatus = serde_json::from_value(
        serde_json::json!({"id":uuid::Uuid::new_v4(),"loop_id":uuid::Uuid::new_v4(),"loop_state":"Idle","instance":foreign,"frequency":"high","accumulated":0,"last_delivery_ms":null,"last_delivery":null,"title":"Shared","link":"Live"}),
    )
    .unwrap();
    let rows = MonitorRowsRegistry::builtin().list(&MonitorContext {
        chat: Some(std::sync::Arc::new(
            tickloop_agent_protocol::ChatStatus::of(&snapshot),
        )),
        subscriptions: vec![only_subscribed],
        member_states: HashMap::from([
            (running.clone(), MemberState::Live),
            (exited.clone(), MemberState::Exited("exit 0".into())),
            (gone.clone(), MemberState::Gone),
            (lost.clone(), MemberState::Lost),
        ]),
        member_resources: HashMap::from([(running.clone(), sample.clone())]),
        ..Default::default()
    });
    let row = |instance: &InstanceRef| {
        rows.iter()
            .find(|row| &row.instance == instance)
            .unwrap()
            .clone()
    };
    let action = |key, label, intent| MonitorAction { key, label, intent };
    let subscribe = |instance: &InstanceRef| {
        action(
            's',
            "subscriptions",
            TickIntent::Subscribe(instance.clone()),
        )
    };

    let live = row(&running);
    assert!(live.running);
    assert_eq!(live.status, "live");
    assert_eq!(live.resources, Some(sample));
    assert_eq!(
        live.actions,
        [
            action('o', "open", TickIntent::OpenView(running.clone())),
            action(
                'x',
                "close",
                TickIntent::CloseHosted {
                    instance: running.clone(),
                    ends_process: true,
                },
            ),
            subscribe(&running),
            action('u', "send update", TickIntent::SendUpdate(running.clone())),
        ],
        "a running Tick this Chat opened closes with x"
    );
    assert_eq!(
        row(&exited).actions,
        [
            action('o', "open", TickIntent::OpenView(exited.clone())),
            action(
                'x',
                "remove",
                TickIntent::CloseHosted {
                    instance: exited.clone(),
                    ends_process: false,
                },
            ),
            subscribe(&exited),
        ],
        "an exited terminal Tick still opens its last screen and leaves with x"
    );
    for (instance, status) in [(&gone, "gone"), (&lost, "lost")] {
        let ended = row(instance);
        assert!(!ended.running);
        assert_eq!(ended.status, status);
        assert_eq!(ended.resources, None);
        assert_eq!(
            ended.actions,
            [
                action('o', "reopen", TickIntent::ReopenHosted(instance.clone())),
                action(
                    'x',
                    "remove",
                    TickIntent::CloseHosted {
                        instance: instance.clone(),
                        ends_process: false,
                    },
                ),
            ],
            "{status}: Enter reopens it in place, x forgets its launch record with the row, and \
             nothing that ended offers subscribing"
        );
    }
    let shared = row(&foreign);
    assert_eq!(
        shared.actions[..3],
        [
            action('o', "open", TickIntent::OpenView(foreign.clone())),
            action(
                'x',
                "remove",
                TickIntent::RemoveFromMonitor(foreign.clone())
            ),
            action('s', "subscriptions", TickIntent::Subscribe(foreign.clone())),
        ],
        "a Tick only this Chat's subscriptions show leaves with them and is never closed; \
         its s cycles their subscriptions"
    );
    assert!(
        rows.iter()
            .flat_map(|row| &row.actions)
            .all(|action| action.key != 'c' && action.key != 'n'),
        "no separate close or reopen key"
    );
    agent.shutdown().await;
}
