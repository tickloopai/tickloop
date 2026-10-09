use crate::support;

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{Value, json};
use tick_protocol::InstanceRef;
use tickloop_agent::{
    host_tools::{
        HostCallResult, HostEffect, HostToolPermission, HostToolProvider, PreparedHostCall,
    },
    monitor_rows::{
        MonitorAction, MonitorContext, MonitorRows, MonitorRowsRegistry, TickDescriptor, TickGlyph,
        TickGroup, TickIntent, TickOwner,
    },
    runtime::{Agent, EventKind, MonitorMemberSource},
    tick_adapter::{TickAdapter, TickAdapters},
    tick_stream::{InMemoryTickStream, TickStreamPort},
};
use tickloop_agent_protocol::{ChatId, ToolDefinition, ToolResult};
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
    fn tick_type(&self) -> &'static str {
        "cal"
    }
    fn stream(&self) -> Option<Arc<dyn TickStreamPort>> {
        Some(Arc::new(self.stream.clone()))
    }
}

/// Lists every member and subscription of its type through the shared Monitor contract.
struct CalendarRows;
impl MonitorRows for CalendarRows {
    fn tick_type(&self) -> &'static str {
        "cal"
    }
    fn list(&self, context: &MonitorContext) -> Vec<TickDescriptor> {
        context
            .subscriptions
            .iter()
            .filter(|row| row.instance.tick_type() == "cal")
            .map(|row| TickDescriptor {
                instance: row.instance.clone(),
                group: TickGroup::Ticks,
                glyph: TickGlyph::Protocol,
                title: row.instance.to_string(),
                kind: "Calendar".into(),
                status: "live".into(),
                owner: TickOwner::Unknown,
                updated_at_ms: row.last_delivery_ms,
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
    agent.submit(chat, None, text.into()).await.unwrap();
    loop {
        match support::next_event(&mut events).await.kind {
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
        chat: Some(agent.snapshot(chat).await.unwrap()),
        subscriptions: agent.tick_subscriptions(chat).await.unwrap(),
        ..Default::default()
    })
}

#[tokio::test]
async fn an_unsubscribed_open_joins_only_its_chat_monitor_without_watch_or_model_work() {
    let directory = tempfile::tempdir().unwrap();
    let model = Arc::new(support::ScriptedModel::new(vec![
        support::call("open", OPEN_TOOL, json!({})),
        support::text("opened"),
    ]));
    let agent = Agent::new_embedded(support::config(), model.clone());
    agent.install_host_tools(Arc::new(OpeningHost)).unwrap();
    agent.install_tick_stream(adapters()).await;
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    let other = agent.create_chat(directory.path().into()).await.unwrap();
    run_turn(&agent, chat, "open the calendar").await;

    let snapshot = agent.snapshot(chat).await.unwrap();
    assert_eq!(snapshot.monitor.members.len(), 1);
    assert_eq!(snapshot.monitor.members[0].instance, calendar("opened"));
    assert_eq!(
        snapshot.monitor.members[0].source,
        MonitorMemberSource::Opened
    );
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
        .branch_chat(tickloop_agent::runtime::BranchRequest {
            request_id: uuid::Uuid::new_v4(),
            source_chat_id: chat,
            source_loop_id: None,
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
async fn removal_outlives_subscription_projection_and_restart_until_explicit_add() {
    let directory = tempfile::tempdir().unwrap();
    let store = directory.path().join("archive");
    let model = Arc::new(support::ScriptedModel::new(vec![
        support::call(
            "subscribe",
            "SubscribeTick",
            json!({"tick_instance_id":"cal:watched","every_seconds":30}),
        ),
        support::text("watching"),
    ]));
    let agent = Agent::open_embedded(store.clone(), support::config(), model).unwrap();
    agent.install_tick_stream(adapters()).await;
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    run_turn(&agent, chat, "watch the calendar").await;
    assert_eq!(monitor_rows(&agent, chat).await.len(), 1);

    agent
        .remove_monitor_member(chat, calendar("watched"))
        .await
        .unwrap();
    assert!(monitor_rows(&agent, chat).await.is_empty());
    assert_eq!(
        agent.tick_subscriptions(chat).await.unwrap().len(),
        1,
        "removal leaves the subscription in place"
    );
    agent.shutdown().await;
    drop(agent);

    let restored = Agent::open_embedded(
        store,
        support::config(),
        Arc::new(support::ScriptedModel::new(Vec::new())),
    )
    .unwrap();
    restored.install_tick_stream(adapters()).await;
    restored.resume_chat(chat).await.unwrap();
    assert!(
        monitor_rows(&restored, chat).await.is_empty(),
        "restart and projection keep the removal"
    );
    restored
        .add_monitor_member(chat, calendar("watched"), None)
        .await
        .unwrap();
    let rows = monitor_rows(&restored, chat).await;
    assert_eq!(rows.len(), 1);
    assert_eq!(
        restored.snapshot(chat).await.unwrap().monitor.members[0].source,
        MonitorMemberSource::Added
    );
    assert_eq!(
        rows[0].owner,
        TickOwner::Unknown,
        "adding never takes ownership"
    );
    restored.shutdown().await;
}

#[tokio::test]
async fn an_empty_chat_keeps_members_and_removals_across_restart() {
    let directory = tempfile::tempdir().unwrap();
    let store = directory.path().join("archive");
    let agent = Agent::open_embedded(
        store.clone(),
        support::config(),
        Arc::new(support::ScriptedModel::new(Vec::new())),
    )
    .unwrap();
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    agent
        .add_monitor_member(chat, calendar("kept"), Some("Kept calendar".into()))
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
        support::config(),
        Arc::new(support::ScriptedModel::new(Vec::new())),
    )
    .unwrap();
    restored.resume_chat(chat).await.unwrap();
    let membership = restored.snapshot(chat).await.unwrap().monitor;
    assert_eq!(membership.members.len(), 1);
    assert_eq!(membership.members[0].instance, calendar("kept"));
    assert_eq!(
        membership.members[0].title.as_deref(),
        Some("Kept calendar")
    );
    assert!(membership.removed.contains(&calendar("hidden")));
    restored.shutdown().await;
}

#[tokio::test(start_paused = true)]
async fn one_row_merges_every_source_and_frequency_and_pages_cover_the_full_set() {
    let directory = tempfile::tempdir().unwrap();
    let model = Arc::new(support::ScriptedModel::new(
        [15, 45]
            .into_iter()
            .flat_map(|every_seconds| {
                vec![
                    support::call(
                        &format!("s{every_seconds}"),
                        "SubscribeTick",
                        json!({"tick_instance_id":"cal:shared","every_seconds":every_seconds}),
                    ),
                    support::text("watching"),
                ]
            })
            .collect(),
    ));
    let agent = Agent::new_embedded(support::config(), model.clone());
    agent.install_tick_stream(adapters()).await;
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    run_turn(&agent, chat, "first watcher").await;
    run_turn(&agent, chat, "second watcher").await;
    let requests = model.requests.lock().unwrap().len();
    agent
        .add_monitor_member(chat, calendar("shared"), Some("Shared calendar".into()))
        .await
        .unwrap();
    for index in 0..200 {
        agent
            .add_monitor_member(chat, calendar(&format!("m{index:03}")), None)
            .await
            .unwrap();
    }
    let context = MonitorContext {
        chat: Some(agent.snapshot(chat).await.unwrap()),
        subscriptions: agent.tick_subscriptions(chat).await.unwrap(),
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
    assert!(
        shared.status.contains("15s") && shared.status.contains("45s"),
        "{}",
        shared.status
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
    agent
        .remove_monitor_member(chat, calendar("m000"))
        .await
        .unwrap();
    let changed = MonitorContext {
        chat: Some(agent.snapshot(chat).await.unwrap()),
        subscriptions: agent.tick_subscriptions(chat).await.unwrap(),
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
    let model = Arc::new(support::ScriptedModel::new(vec![
        support::call("open", OPEN_TOOL, json!({})),
        support::text("opened"),
        support::text("second reply"),
        support::text("summary of the conversation"),
        support::text("third reply"),
    ]));
    let agent = Agent::open_embedded(store.clone(), support::config(), model).unwrap();
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
            tickloop_agent::runtime::RewindMode::Conversation,
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
        support::config(),
        Arc::new(support::ScriptedModel::new(Vec::new())),
    )
    .unwrap();
    restored.resume_chat(chat).await.unwrap();
    assert_eq!(restored.snapshot(chat).await.unwrap().monitor, before);
    restored.shutdown().await;
}

/// A hosted member shows what the latest probe found: running with its process sample, gone,
/// or lost with its host; every row offers Close, and a lost one also Reopen as new.
#[tokio::test]
async fn hosted_members_show_their_probed_state_and_sample_and_offer_close() {
    use std::collections::HashMap;
    use tickloop_agent::monitor_rows::{MemberState, TickResources};
    let directory = tempfile::tempdir().unwrap();
    let agent = Agent::new_embedded(
        support::config(),
        Arc::new(support::ScriptedModel::new(Vec::new())),
    );
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    let (running, gone, lost) = (
        InstanceRef::hosted(uuid::Uuid::new_v4()),
        InstanceRef::hosted(uuid::Uuid::new_v4()),
        InstanceRef::hosted(uuid::Uuid::new_v4()),
    );
    for instance in [&running, &gone, &lost] {
        agent
            .add_monitor_member(chat, instance.clone(), Some("Counter".into()))
            .await
            .unwrap();
    }
    let sample = TickResources {
        pid: 4242,
        cpu_time_ms: Some(120),
        cpu_percent: None,
        rss_bytes: Some(8 << 20),
        sampled_at_ms: 1,
    };
    let rows = MonitorRowsRegistry::builtin().list(&MonitorContext {
        chat: Some(agent.snapshot(chat).await.unwrap()),
        member_states: HashMap::from([
            (running.clone(), MemberState::Live { protocol: true }),
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
    let live = row(&running);
    assert!(live.running);
    assert_eq!(live.status, "live");
    assert_eq!(live.resources, Some(sample));
    let ended = row(&gone);
    assert!(!ended.running);
    assert_eq!(ended.status, "gone");
    assert_eq!(ended.resources, None);
    let hosted_actions = |instance: &InstanceRef| {
        vec![
            MonitorAction {
                key: 'o',
                label: "open",
                intent: TickIntent::OpenHosted(instance.clone()),
            },
            MonitorAction {
                key: 's',
                label: "snapshot",
                intent: TickIntent::ShowHostedSnapshot(instance.clone()),
            },
            MonitorAction {
                key: 'x',
                label: "remove",
                intent: TickIntent::RemoveFromMonitor(instance.clone()),
            },
            MonitorAction {
                key: 'c',
                label: "close",
                intent: TickIntent::CloseHosted(instance.clone()),
            },
        ]
    };
    assert_eq!(live.actions, hosted_actions(&running));
    assert_eq!(ended.actions, hosted_actions(&gone));
    let recoverable = row(&lost);
    assert_eq!(recoverable.status, "lost");
    let mut reopenable = hosted_actions(&lost);
    reopenable.push(MonitorAction {
        key: 'n',
        label: "reopen as new",
        intent: TickIntent::ReopenHosted(lost.clone()),
    });
    assert_eq!(
        recoverable.actions, reopenable,
        "a lost Tick can also be reopened as new"
    );
    agent.shutdown().await;
}
