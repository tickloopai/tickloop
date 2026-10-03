mod support;

use serde_json::json;
use std::{sync::Arc, time::Duration};
use support::*;
use tickloop_agent::{
    host_tools::{HostToolPermission, HostToolProvider},
    model::ToolDefinition,
    runtime::{Agent, EventKind, ShellTaskCenter, TickSubscriptionChange},
    tick_adapter::{
        ShellAdapter, TickAdapter, TickAdapters, TickDescriptor, TickGlyph, TickGroup,
        TickListAction, TickListContext, TickListOperation,
    },
    tick_stream::{
        InMemoryTickStream, InstanceRef, ObservationKind, TickObservation, TickStreamPort,
    },
    tools::ToolResult,
};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

struct CalendarAdapter {
    stream: InMemoryTickStream,
}
#[async_trait::async_trait]
impl HostToolProvider for CalendarAdapter {
    fn definitions(&self) -> Vec<ToolDefinition> {
        vec![]
    }
    fn permission(&self, _: Uuid, _: &str, _: &serde_json::Value) -> HostToolPermission {
        HostToolPermission::ReadOnly
    }
    async fn execute(
        &self,
        _: Uuid,
        _: &str,
        _: serde_json::Value,
        _: CancellationToken,
    ) -> ToolResult {
        ToolResult {
            content: json!({"tools":[{"name":"meetings","mutates":false}]}).to_string(),
            is_error: false,
            output_ref: None,
        }
    }
}
impl TickAdapter for CalendarAdapter {
    fn tick_type(&self) -> &'static str {
        "cal"
    }
    fn list(&self, _: &TickListContext) -> Vec<TickDescriptor> {
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
            updated_at_ms: None,
            running: true,
            actions: vec![TickListAction {
                key: 'o',
                label: "open",
                operation: TickListOperation::Notify("Calendar opened".into()),
            }],
        }]
    }
    fn stream(&self) -> Option<Arc<dyn TickStreamPort>> {
        Some(Arc::new(self.stream.clone()))
    }
    fn tools(&self) -> Option<Arc<dyn HostToolProvider>> {
        Some(Arc::new(CalendarAdapter {
            stream: self.stream.clone(),
        }))
    }
}
fn calendar() -> InstanceRef {
    InstanceRef::new("cal", "a").unwrap()
}
async fn complete(events: &mut tokio::sync::broadcast::Receiver<tickloop_agent::runtime::Event>) {
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
async fn a_registered_type_lists_reads_subscribes_and_triggers_without_type_branches() {
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
    agent.install_tick_stream(adapters.clone()).await;
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    let tools = adapters.host_tools(None);
    let call = tools
        .prepare_call(
            chat,
            "ReadTick",
            &json!({"tick_instance_id":calendar()}),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    let read = tools
        .execute_prepared(chat, call, CancellationToken::new())
        .await
        .into_tool_result();
    assert!(!read.is_error);
    assert!(read.content.contains("meetings"));
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
    let rows = adapters.list(&TickListContext {
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
        TickAdapters::new([
            Arc::new(ShellAdapter::new(Some(agent.clone()))) as Arc<dyn TickAdapter>
        ])
        .unwrap(),
    );
    agent.install_tick_stream(adapters.clone()).await;
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
    let rows = adapters.list(&TickListContext {
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
async fn shell_catalog_queries_and_stop_keep_the_existing_task_boundary() {
    let directory = tempfile::tempdir().unwrap();
    let agent = Agent::new_embedded(config(), Arc::new(ScriptedModel::new(vec![])));
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    let task = agent
        .submit_user_shell_for_goal(
            chat,
            None,
            "printf 'recoverable output'; while :; do sleep 0.02; done".into(),
            true,
        )
        .await
        .unwrap();
    let center = ShellTaskCenter::new(agent.clone());
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if center
                .snapshot(chat)
                .await
                .unwrap()
                .tasks
                .iter()
                .any(|row| row.id == task.command_id && row.output_bytes >= 18)
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    let adapters = TickAdapters::builtin(Some(agent.clone()), None, None);
    let tools = adapters.host_tools(None);
    let instance = InstanceRef::shell(task.command_id);
    let read = tools
        .prepare_call(
            chat,
            "ReadTick",
            &json!({"tick_instance_id":instance}),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    let read = tools
        .execute_prepared(chat, read, CancellationToken::new())
        .await
        .into_tool_result();
    let data: serde_json::Value = serde_json::from_str(&read.content).unwrap();
    assert_eq!(data["tools"][0]["name"], "output");
    assert_eq!(data["tools"][1]["name"], "stop");
    assert_eq!(data["tools"][1]["mutates"], true);
    let output = tools
        .prepare_call(
            chat,
            "CallTickTool",
            &json!({"tick_instance_id":instance,"tool":"output","args":{"offset":0,"limit":64}}),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(output.permission(), HostToolPermission::ReadOnly);
    let output = tools
        .execute_prepared(chat, output, CancellationToken::new())
        .await
        .into_tool_result();
    assert!(output.content.contains("recoverable output"));
    assert!(
        tools
            .prepare_call(
                chat,
                "CallTickTool",
                &json!({"tick_instance_id":instance,"tool":"output","args":{"limit":0}}),
                CancellationToken::new()
            )
            .await
            .is_err()
    );
    let other = agent.create_chat(directory.path().into()).await.unwrap();
    assert!(
        tools
            .prepare_call(
                other,
                "ReadTick",
                &json!({"tick_instance_id":instance}),
                CancellationToken::new()
            )
            .await
            .is_err()
    );
    let cancelled_call = tools
        .prepare_call(
            chat,
            "CallTickTool",
            &json!({"tick_instance_id":instance,"tool":"output","args":{}}),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    let cancelled = CancellationToken::new();
    cancelled.cancel();
    assert!(matches!(
        tools
            .execute_prepared(chat, cancelled_call, cancelled)
            .await,
        tickloop_agent::host_tools::HostCallResult::NotDispatched(_)
    ));
    assert!(
        tools
            .prepare_call(
                chat,
                "CallTickTool",
                &json!({"tick_instance_id":instance,"tool":"stop","args":{}}),
                CancellationToken::new()
            )
            .await
            .is_err(),
        "direct host dispatch must not bypass TaskStop's caller loop boundary"
    );
    assert!(
        !center
            .snapshot(chat)
            .await
            .unwrap()
            .tasks
            .iter()
            .find(|row| row.id == task.command_id)
            .unwrap()
            .state
            .is_terminal()
    );
    agent.shutdown().await;
}

#[tokio::test]
async fn shell_output_changes_before_exit_and_a_late_watch_uses_a_new_baseline() {
    use tickloop_agent::tick_stream::TickStreamEvent;
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
    let adapters = TickAdapters::builtin(Some(agent.clone()), None, None);
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

#[tokio::test]
async fn shell_call_tick_tool_rejects_an_undeclared_read_name() {
    let directory = tempfile::tempdir().unwrap();
    let agent = Agent::new_embedded(config(), Arc::new(ScriptedModel::new(vec![])));
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    let task = agent
        .submit_user_shell_for_goal(chat, None, "sleep 30".into(), true)
        .await
        .unwrap();
    let tools = TickAdapters::builtin(Some(agent.clone()), None, None).host_tools(None);
    assert!(tools.prepare_call(chat,"CallTickTool",&json!({"tick_instance_id":InstanceRef::shell(task.command_id),"tool":"read","args":{"ignored":true}}),CancellationToken::new()).await.is_err());
    agent.shutdown().await;
}

async fn stop_fixture() -> (
    tempfile::TempDir,
    Agent,
    Arc<ScriptedModel>,
    Uuid,
    Uuid,
    Uuid,
) {
    use tickloop_agent::permissions::ExecutionMode;
    let directory = tempfile::tempdir().unwrap();
    let model = Arc::new(ScriptedModel::new(vec![
        call(
            "start",
            "Bash",
            json!({"command":"sleep 30","run_in_background":true}),
        ),
        text("started"),
    ]));
    let agent = Agent::new_embedded(config(), model.clone());
    let adapters = TickAdapters::builtin(Some(agent.clone()), None, None);
    agent.install_host_tools(adapters.host_tools(None)).unwrap();
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    agent
        .set_execution_mode(chat, ExecutionMode::FullAccess)
        .await
        .unwrap();
    let mut events = agent.subscribe();
    let receipt = agent
        .submit(chat, None, "start a background command".into())
        .await
        .unwrap();
    complete(&mut events).await;
    let task = agent.commands(chat).await.unwrap().remove(0).operation_id;
    (directory, agent, model, chat, receipt.loop_id, task)
}
fn enqueue_stop(model: &ScriptedModel, task: Uuid) {
    model.enqueue(call(
        "adapter-stop",
        "CallTickTool",
        json!({"tick_instance_id":InstanceRef::shell(task),"tool":"stop","args":{}}),
    ));
    model.enqueue(text("stop result received"));
}
#[tokio::test]
async fn adapter_stop_refuses_other_loops_and_user_shells() {
    let (_directory, agent, model, chat, _, task) = stop_fixture().await;
    let mut events = agent.subscribe();
    enqueue_stop(&model, task);
    agent
        .submit(chat, None, "stop another goal's command".into())
        .await
        .unwrap();
    complete(&mut events).await;
    assert!(!command_finished(
        &agent
            .commands(chat)
            .await
            .unwrap()
            .iter()
            .find(|row| row.operation_id == task)
            .unwrap()
            .state
    ));
    let user = agent
        .submit_user_shell_for_goal(chat, None, "sleep 30".into(), true)
        .await
        .unwrap();
    enqueue_stop(&model, user.command_id);
    agent
        .submit(chat, None, "stop the user shell through the model".into())
        .await
        .unwrap();
    complete(&mut events).await;
    assert!(
        !ShellTaskCenter::new(agent.clone())
            .snapshot(chat)
            .await
            .unwrap()
            .tasks
            .iter()
            .find(|row| row.id == user.command_id)
            .unwrap()
            .state
            .is_terminal()
    );
    agent.shutdown().await;
}
#[tokio::test]
async fn adapter_stop_obeys_task_stop_deny_and_ask_rules() {
    use tickloop_agent::permissions::{Decision, PermissionRule};
    for decision in [Decision::Deny, Decision::Ask] {
        let (_directory, agent, model, chat, goal, task) = stop_fixture().await;
        agent
            .set_permission_rules(
                chat,
                vec![PermissionRule {
                    id: Uuid::new_v4(),
                    decision,
                    tool: "TaskStop".into(),
                    specifier: None,
                    input: None,
                    loop_id: None,
                    persistent: false,
                }],
            )
            .await
            .unwrap();
        let mut events = agent.subscribe();
        enqueue_stop(&model, task);
        agent
            .submit(chat, Some(goal), "stop this goal's command".into())
            .await
            .unwrap();
        let mut asked = false;
        loop {
            match next_event(&mut events).await.kind {
                EventKind::PermissionRequested { request } => {
                    assert_eq!(request.name, "TaskStop");
                    asked = true;
                    agent
                        .answer_permission(chat, request.id, true)
                        .await
                        .unwrap();
                }
                EventKind::Completed { .. } => break,
                _ => {}
            }
        }
        let command = agent
            .commands(chat)
            .await
            .unwrap()
            .into_iter()
            .find(|row| row.operation_id == task)
            .unwrap();
        assert_eq!(asked, decision == Decision::Ask);
        assert_eq!(command_finished(&command.state), decision == Decision::Ask);
        agent.shutdown().await;
    }
}
#[tokio::test]
async fn adapter_stop_has_the_same_plan_behavior_as_task_stop() {
    use tickloop_agent::permissions::ExecutionMode;
    let (_directory, agent, model, chat, goal, task) = stop_fixture().await;
    agent
        .set_execution_mode(chat, ExecutionMode::Plan)
        .await
        .unwrap();
    enqueue_stop(&model, task);
    let mut events = agent.subscribe();
    agent
        .submit(chat, Some(goal), "stop this goal's command in Plan".into())
        .await
        .unwrap();
    complete(&mut events).await;
    assert!(command_finished(
        &agent
            .commands(chat)
            .await
            .unwrap()
            .iter()
            .find(|row| row.operation_id == task)
            .unwrap()
            .state
    ));
    agent.shutdown().await;
}

fn command_finished(state: &tickloop_agent::tools::CommandState) -> bool {
    matches!(
        state,
        tickloop_agent::tools::CommandState::Completed
            | tickloop_agent::tools::CommandState::Failed
            | tickloop_agent::tools::CommandState::Unknown
    )
}

#[test]
fn duplicate_instance_projection_keeps_the_newest_delivery_in_any_subscription_order() {
    use tickloop_agent::runtime::TickSubscriptionStatus;
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
        let rows = TickAdapters::presentation().list(&TickListContext {
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
async fn shell_subscription_refuses_the_same_foreign_chat_as_its_query_aliases() {
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
    let adapters = Arc::new(TickAdapters::builtin(Some(agent.clone()), None, None));
    let tools = adapters.host_tools(None);
    let instance = InstanceRef::shell(task.command_id);
    for (name, input) in [
        ("ReadTick", json!({"tick_instance_id":instance})),
        (
            "CallTickTool",
            json!({"tick_instance_id":instance,"tool":"output","args":{}}),
        ),
    ] {
        assert!(
            tools
                .prepare_call(observer, name, &input, CancellationToken::new())
                .await
                .is_err()
        );
    }
    agent.install_host_tools(tools).unwrap();
    agent.install_tick_stream(adapters).await;
    model.enqueue(call(
        "foreign-read",
        "ReadTick",
        json!({"tick_instance_id":instance}),
    ));
    model.enqueue(call(
        "foreign-output",
        "CallTickTool",
        json!({"tick_instance_id":instance,"tool":"output","args":{}}),
    ));
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
    assert_eq!(refused, 3);
    assert!(
        agent.tick_subscriptions(observer).await.unwrap().is_empty(),
        "foreign Shell watch became configured"
    );
    agent
        .configure_trigger(
            observer,
            receipt.loop_id,
            Some(tickloop_agent::runtime::Trigger {
                id: Uuid::new_v4(),
                prompt: "restored foreign watch".into(),
                interval_seconds: None,
                expires_at_ms: None,
                accepts_events: true,
                armed: true,
                subscriptions: vec![tickloop_agent::runtime::TickSubscription {
                    id: Uuid::new_v4(),
                    instance: instance.clone(),
                    every_seconds: 5,
                }],
            }),
        )
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let rows = agent.tick_subscriptions(observer).await.unwrap();
            if rows[0].link == tickloop_agent::runtime::TickLinkState::IdentityUncertain {
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
    let tick = tickloop_agent::artifacts::ArtifactTick {
        artifact_id: "doc".into(),
        origin: "https://docs.example".into(),
        title: "Joined title".into(),
        format: "markdown".into(),
        url: "https://docs.example/a/doc".into(),
        access_role: "owner".into(),
        added_at_ms: 1,
    };
    agent.add_artifact_tick(chat, tick.clone()).await.unwrap();
    let row:tickloop_agent::runtime::TickSubscriptionStatus=serde_json::from_value(json!({"id":Uuid::new_v4(),"loop_id":Uuid::new_v4(),"loop_state":"WaitingTrigger","instance":tick.instance_id(),"every_seconds":60,"accumulated":0,"last_delivery_ms":null,"last_delivery":null,"kind":"Protocol","title":null,"updated_at_ms":null,"link":"Disconnected"})).unwrap();
    let mut context = TickListContext {
        chat: Some(agent.snapshot(chat).await.unwrap()),
        subscriptions: vec![row],
        ..Default::default()
    };
    let registry = TickAdapters::presentation();
    assert_eq!(registry.list(&context)[0].title, "Joined title");
    context.metadata.insert(tick.instance_id(),Some(serde_json::from_value(json!({"id":"doc","title":"Cached title","format":"markdown","access_role":"owner"})).unwrap()));
    context
        .metadata_failures
        .insert(tick.instance_id(), "refresh failed".into());
    assert_eq!(registry.list(&context)[0].title, "Cached title");
    agent.shutdown().await;
}

#[tokio::test]
async fn shell_and_new_adapter_aliases_preserve_read_policy_including_final_admission() {
    use support::tick_alias_contract::{AliasCase, verify_alias_policy};
    let directory = tempfile::tempdir().unwrap();
    for use_shell in [true, false] {
        let model = Arc::new(ScriptedModel::new(vec![]));
        let agent = Agent::new_embedded(config(), model.clone());
        let chat = agent.create_chat(directory.path().into()).await.unwrap();
        let (adapters, instance, tool) = if use_shell {
            let task = agent
                .submit_user_shell_for_goal(chat, None, "printf shell-body".into(), true)
                .await
                .unwrap();
            (
                TickAdapters::builtin(Some(agent.clone()), None, None),
                InstanceRef::shell(task.command_id),
                "output",
            )
        } else {
            (
                TickAdapters::new([Arc::new(CalendarAdapter {
                    stream: InMemoryTickStream::new(),
                }) as Arc<dyn TickAdapter>])
                .unwrap(),
                calendar(),
                "meetings",
            )
        };
        let adapters = Arc::new(adapters);
        let provider = adapters.host_tools(None);
        let read_input = json!({"tick_instance_id":instance});
        let query_input = json!({"tick_instance_id":instance,"tool":tool,"args":{}});
        let cases = [
            AliasCase {
                plan_denied: false,
                name: "ReadTick",
                input: read_input.clone(),
                policy_name: "ReadTick",
                policy_input: read_input.clone(),
            },
            AliasCase {
                plan_denied: false,
                name: "CallTickTool",
                input: query_input.clone(),
                policy_name: "CallTickTool",
                policy_input: query_input,
            },
            AliasCase {
                plan_denied: false,
                name: "SubscribeTick",
                input: json!({"tick_instance_id":instance,"every_seconds":5}),
                policy_name: "ReadTick",
                policy_input: read_input,
            },
        ];
        agent.install_host_tools(provider).unwrap();
        agent.install_tick_stream(adapters).await;
        verify_alias_policy(&agent, &model, chat, &cases).await;
        agent.shutdown().await;
    }
}
