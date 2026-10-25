//! Behavioral contracts shared by execution environment Adapters.
use serde_json::json;
use std::sync::Arc;
use tick_protocol::InstanceRef;
use tickloop_harness_runtime::environment::*;
use tokio_util::sync::CancellationToken;

/// The same exit/observation/output contract is used by process-backed and memory Adapters.
pub async fn shell_exit_contract<F, Fut>(
    environment: &dyn ExecutionEnvironment,
    scope: ExecutionScope,
    spec: ShellSpec,
    release: F,
) where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = ()>,
{
    let authority = ExecutionAuthority::new(scope.chat);
    let port = environment.bind(scope, authority.clone()).unwrap();
    let prepared = port
        .prepare(ExecutionIntent::SpawnShell(spec))
        .await
        .unwrap();
    let admission = authority.admit(&prepared).unwrap();
    let ExecutionOutcome::Completed(started) = port
        .execute(prepared, admission, CancellationToken::new())
        .await
    else {
        panic!("Shell was not started");
    };
    let instance = started.shell.unwrap().instance;
    let mut observation = port
        .observe(ObserveRequest {
            instance: instance.clone(),
            after: None,
        })
        .await
        .unwrap();
    assert_eq!(observation.baseline.state.data["state"], "Running");
    release().await;
    tokio::time::timeout(
        std::time::Duration::from_secs(30),
        observation.updates.changed(),
    )
    .await
    .unwrap()
    .unwrap();
    let update = observation.updates.borrow_and_update().clone();
    assert_eq!(update.update.unwrap().data["state"], "Completed");
    let late = port
        .observe(ObserveRequest {
            instance: instance.clone(),
            after: None,
        })
        .await
        .unwrap();
    assert_eq!(late.baseline.state.data["state"], "Completed");
    assert!(!late.updates.has_changed().unwrap());
    let ExecutionInspection::Output(output) = port
        .inspect(ExecutionQuery::ShellOutput {
            instance,
            offset: 0,
            limit: 1024,
        })
        .await
        .unwrap()
    else {
        panic!("missing Shell output");
    };
    assert_eq!(output.data, b"done\n");
    assert!(output.eof);
    assert!(!observation.updates.has_changed().unwrap());
}

pub async fn shell_stop_contract(
    environment: &dyn ExecutionEnvironment,
    scope: ExecutionScope,
    spec: ShellSpec,
) {
    let authority = ExecutionAuthority::new(scope.chat);
    let port = environment.bind(scope, authority.clone()).unwrap();
    let prepared = port
        .prepare(ExecutionIntent::SpawnShell(spec))
        .await
        .unwrap();
    let admission = authority.admit(&prepared).unwrap();
    let ExecutionOutcome::Completed(started) = port
        .execute(prepared, admission, CancellationToken::new())
        .await
    else {
        panic!("Shell was not started");
    };
    let instance = started.shell.unwrap().instance;
    let mut observation = port
        .observe(ObserveRequest {
            instance: instance.clone(),
            after: None,
        })
        .await
        .unwrap();
    let prepared = port
        .prepare(ExecutionIntent::StopShell {
            instance: instance.clone(),
            reason: "contract-stop".into(),
        })
        .await
        .unwrap();
    let admission = authority.admit(&prepared).unwrap();
    assert!(matches!(
        port.execute(prepared, admission, CancellationToken::new())
            .await,
        ExecutionOutcome::Completed(_)
    ));
    tokio::time::timeout(
        std::time::Duration::from_secs(30),
        observation.updates.changed(),
    )
    .await
    .unwrap()
    .unwrap();
    let ExecutionInspection::Shell(status) = port
        .inspect(ExecutionQuery::ShellStatus(instance))
        .await
        .unwrap()
    else {
        panic!("missing Shell status");
    };
    assert_eq!(
        status.state,
        tickloop_harness_protocol::CommandState::Failed
    );
    assert_eq!(status.reason.as_deref(), Some("contract-stop"));
}

pub async fn dispatched_cancellation_contract(
    environment: &dyn ExecutionEnvironment,
    scope: ExecutionScope,
    open: OpenSpec,
) {
    for revoke in [false, true] {
        dispatched_interruption(environment, scope.clone(), open.clone(), revoke).await;
    }
}

async fn dispatched_interruption(
    environment: &dyn ExecutionEnvironment,
    scope: ExecutionScope,
    open: OpenSpec,
    revoke: bool,
) {
    let authority = ExecutionAuthority::new(scope.chat);
    let port = environment.bind(scope.clone(), authority.clone()).unwrap();
    let prepared = port.prepare(ExecutionIntent::Open(open)).await.unwrap();
    let admission = authority.admit(&prepared).unwrap();
    let ExecutionOutcome::Completed(opened) = port
        .execute(prepared, admission, CancellationToken::new())
        .await
    else {
        panic!("Tick was not opened");
    };
    let instance = opened.opened.unwrap().instance;
    let mut observed = port
        .observe(ObserveRequest {
            instance: instance.clone(),
            after: None,
        })
        .await
        .unwrap();
    let prepared = port
        .prepare(ExecutionIntent::Call {
            instance: instance.clone(),
            tool: "increment".into(),
            args: json!({}),
            revision: "1".into(),
        })
        .await
        .unwrap();
    let operation = prepared.operation();
    let admission = authority.admit(&prepared).unwrap();
    let cancel = CancellationToken::new();
    let pending = tokio::spawn({
        let port = port.clone();
        let cancel = cancel.clone();
        async move { port.execute(prepared, admission, cancel).await }
    });
    tokio::time::timeout(std::time::Duration::from_secs(30), async {
        loop {
            observed.updates.changed().await.unwrap();
            if let Some(update) = &observed.updates.borrow_and_update().update {
                assert_eq!(update.data["value"], 1);
                break;
            }
        }
    })
    .await
    .unwrap();
    if revoke {
        authority.revoke();
    } else {
        cancel.cancel();
    }
    let outcome = tokio::time::timeout(std::time::Duration::from_secs(2), pending)
        .await
        .expect("cancellation did not release the dispatched operation")
        .unwrap();
    assert!(
        matches!(outcome, ExecutionOutcome::OutcomeUnknown { operation: id, .. } if id == operation)
    );
    let (port, authority) = if revoke {
        assert!(
            port.inspect(ExecutionQuery::Read(instance.clone()))
                .await
                .is_err()
        );
        let next = ExecutionAuthority::new(scope.chat);
        (environment.bind(scope, next.clone()).unwrap(), next)
    } else {
        (port, authority)
    };
    for _ in 0..2 {
        assert!(matches!(
            port.inspect(ExecutionQuery::Receipt(operation))
                .await
                .unwrap(),
            ExecutionInspection::Receipt(_)
        ));
    }
    let ExecutionInspection::Read(read) = port
        .inspect(ExecutionQuery::Read(instance.clone()))
        .await
        .unwrap()
    else {
        panic!("missing counter state");
    };
    assert_eq!(
        read.state.data["value"], 1,
        "a cancelled dispatched mutation must not run again"
    );
    let close = port
        .prepare(ExecutionIntent::Close { instance })
        .await
        .unwrap();
    let admission = authority.admit(&close).unwrap();
    assert!(matches!(
        port.execute(close, admission, CancellationToken::new())
            .await,
        ExecutionOutcome::Completed(_)
    ));
}

async fn open(
    environment: &dyn ExecutionEnvironment,
    scope: ExecutionScope,
    spec: OpenSpec,
) -> (Arc<dyn ExecutionPort>, ExecutionAuthority, ExecutionResult) {
    let authority = ExecutionAuthority::new(scope.chat);
    let port = environment.bind(scope, authority.clone()).unwrap();
    let result = execute(&port, &authority, ExecutionIntent::Open(spec)).await;
    let ExecutionOutcome::Completed(result) = result else {
        panic!("open did not complete: {result:?}");
    };
    (port, authority, result)
}

async fn execute(
    port: &Arc<dyn ExecutionPort>,
    authority: &ExecutionAuthority,
    intent: ExecutionIntent,
) -> ExecutionOutcome {
    let prepared = port.prepare(intent).await.unwrap();
    let admission = authority.admit(&prepared).unwrap();
    port.execute(prepared, admission, CancellationToken::new())
        .await
}

pub async fn program_change_contract(
    environment: &dyn ExecutionEnvironment,
    scope: ExecutionScope,
    spec: OpenSpec,
    replace: impl FnOnce(),
) {
    let authority = ExecutionAuthority::new(scope.chat);
    let port = environment.bind(scope, authority.clone()).unwrap();
    let prepared = port.prepare(ExecutionIntent::Open(spec)).await.unwrap();
    let admission = authority.admit(&prepared).unwrap();
    replace();
    assert!(
        matches!(port.execute(prepared, admission, CancellationToken::new()).await,
        ExecutionOutcome::NotDispatched(error) if error.message.contains("changed before dispatch"))
    );
}

pub async fn authority_contract(
    environment: &dyn ExecutionEnvironment,
    scope: ExecutionScope,
    spec: OpenSpec,
) {
    let authority = ExecutionAuthority::new(scope.chat);
    let foreign = ExecutionAuthority::new(tickloop_ids::generate().into());
    assert!(environment.bind(scope.clone(), foreign).is_err());
    let port = environment.bind(scope, authority.clone()).unwrap();
    let prepared = port
        .prepare(ExecutionIntent::Open(spec.clone()))
        .await
        .unwrap();
    let admission = authority.admit(&prepared).unwrap();
    authority.revoke();
    assert!(
        matches!(port.execute(prepared, admission, CancellationToken::new()).await, ExecutionOutcome::NotDispatched(error) if error.code == "AuthorityRevoked")
    );
    assert!(port.inspect(ExecutionQuery::Capabilities).await.is_err());
    assert!(port.prepare(ExecutionIntent::Open(spec)).await.is_err());
    assert!(
        port.observe(ObserveRequest {
            instance: InstanceRef::hosted(tickloop_ids::generate()),
            after: None
        })
        .await
        .is_err()
    );
}

pub async fn ownership_contract(
    environment: &dyn ExecutionEnvironment,
    scope: ExecutionScope,
    foreign_environment: &dyn ExecutionEnvironment,
    foreign_scope: ExecutionScope,
    spec: OpenSpec,
) {
    let (port, authority, opened) = open(environment, scope, spec).await;
    let instance = opened.opened.unwrap().instance;
    let other = ExecutionAuthority::new(foreign_scope.chat);
    let foreign = foreign_environment
        .bind(foreign_scope, other.clone())
        .unwrap();
    assert!(
        foreign
            .inspect(ExecutionQuery::Read(instance.clone()))
            .await
            .is_err()
    );
    if let Ok(prepared) = foreign
        .prepare(ExecutionIntent::Close {
            instance: instance.clone(),
        })
        .await
    {
        let admission = other.admit(&prepared).unwrap();
        assert!(matches!(
            foreign
                .execute(prepared, admission, CancellationToken::new())
                .await,
            ExecutionOutcome::NotDispatched(_)
        ));
    }
    assert!(
        port.inspect(ExecutionQuery::Read(instance.clone()))
            .await
            .is_ok()
    );
    assert!(matches!(
        execute(&port, &authority, ExecutionIntent::Close { instance }).await,
        ExecutionOutcome::Completed(_)
    ));
}

pub async fn revision_contract<F, Fut>(
    environment: &dyn ExecutionEnvironment,
    scope: ExecutionScope,
    spec: OpenSpec,
    evolve: F,
) where
    F: FnOnce(InstanceRef) -> Fut,
    Fut: std::future::Future<Output = ()>,
{
    let (port, authority, opened) = open(environment, scope, spec).await;
    let instance = opened.opened.unwrap().instance;
    let prepared = port
        .prepare(ExecutionIntent::Call {
            instance: instance.clone(),
            tool: "increment".into(),
            args: json!({}),
            revision: "1".into(),
        })
        .await
        .unwrap();
    let admission = authority.admit(&prepared).unwrap();
    evolve(instance.clone()).await;
    assert!(
        matches!(port.execute(prepared, admission, CancellationToken::new()).await, ExecutionOutcome::NotDispatched(error) if error.message.contains("catalog changed"))
    );
    let ExecutionInspection::Read(read) = port
        .inspect(ExecutionQuery::Read(instance.clone()))
        .await
        .unwrap()
    else {
        panic!("missing state");
    };
    assert_eq!(read.state.data["value"], 0);
    assert!(matches!(
        execute(&port, &authority, ExecutionIntent::Close { instance }).await,
        ExecutionOutcome::Completed(_)
    ));
}

pub async fn observation_contract(
    environment: &dyn ExecutionEnvironment,
    scope: ExecutionScope,
    spec: OpenSpec,
) {
    let (port, authority, opened) = open(environment, scope, spec).await;
    let instance = opened.opened.unwrap().instance;
    let mut first = port
        .observe(ObserveRequest {
            instance: instance.clone(),
            after: None,
        })
        .await
        .unwrap();
    assert_eq!(first.baseline.state.data["value"], 0);
    assert!(matches!(
        execute(
            &port,
            &authority,
            ExecutionIntent::Call {
                instance: instance.clone(),
                tool: "increment".into(),
                args: json!({"amount":1}),
                revision: "1".into()
            }
        )
        .await,
        ExecutionOutcome::Completed(_)
    ));
    let mut late = port
        .observe(ObserveRequest {
            instance: instance.clone(),
            after: None,
        })
        .await
        .unwrap();
    assert_eq!(late.baseline.state.data["value"], 1);
    assert!(matches!(
        execute(
            &port,
            &authority,
            ExecutionIntent::Call {
                instance: instance.clone(),
                tool: "increment".into(),
                args: json!({"amount":1}),
                revision: "1".into()
            }
        )
        .await,
        ExecutionOutcome::Completed(_)
    ));
    for observation in [&mut first, &mut late] {
        let latest = tokio::time::timeout(
            std::time::Duration::from_secs(30),
            observation.updates.wait_for(|update| {
                update
                    .update
                    .as_ref()
                    .is_some_and(|state| state.data["value"] == 2)
            }),
        )
        .await
        .unwrap()
        .unwrap()
        .clone();
        assert!(latest.received > observation.cursor.position);
        assert_eq!(latest.update.unwrap().data["value"], 2);
    }
    let resumed = port
        .observe(ObserveRequest {
            instance: instance.clone(),
            after: Some(first.cursor),
        })
        .await
        .unwrap();
    assert_eq!(resumed.baseline.state.data["value"], 2);
    assert!(matches!(
        execute(&port, &authority, ExecutionIntent::Close { instance }).await,
        ExecutionOutcome::Completed(_)
    ));
}

pub async fn restart_contract<F, Fut>(
    environment: &dyn ExecutionEnvironment,
    scope: ExecutionScope,
    spec: OpenSpec,
    restart: F,
) where
    F: FnOnce(serde_json::Value) -> Fut,
    Fut: std::future::Future<Output = ()>,
{
    let (port, authority, opened) = open(environment, scope, spec).await;
    let instance = opened.opened.unwrap().instance;
    let mut observation = port
        .observe(ObserveRequest {
            instance: instance.clone(),
            after: None,
        })
        .await
        .unwrap();
    restart(serde_json::from_str(&opened.result.content).unwrap()).await;
    let _ = tokio::time::timeout(
        std::time::Duration::from_secs(30),
        observation.updates.changed(),
    )
    .await
    .unwrap();
    assert_ne!(
        observation.updates.borrow_and_update().state,
        ExecutionLinkState::Live
    );
    assert!(
        port.inspect(ExecutionQuery::Kind(instance.clone()))
            .await
            .is_err()
    );
    let ExecutionOutcome::Completed(reopened) = execute(
        &port,
        &authority,
        ExecutionIntent::Reopen {
            instance: instance.clone(),
        },
    )
    .await
    else {
        panic!("explicit reopen failed");
    };
    let replacement = reopened.opened.unwrap().instance;
    assert_ne!(instance, replacement);
    assert!(
        port.observe(ObserveRequest {
            instance: instance.clone(),
            after: Some(observation.cursor)
        })
        .await
        .is_err()
    );
    assert!(port.inspect(ExecutionQuery::Read(instance)).await.is_err());
    assert!(matches!(
        execute(
            &port,
            &authority,
            ExecutionIntent::Close {
                instance: replacement
            }
        )
        .await,
        ExecutionOutcome::Completed(_)
    ));
}

pub async fn view_failure_contract(
    environment: &dyn ExecutionEnvironment,
    scope: ExecutionScope,
    spec: OpenSpec,
) {
    let (port, authority, opened) = open(environment, scope, spec).await;
    let instance = opened.opened.unwrap().instance;
    let value: serde_json::Value = serde_json::from_str(&opened.result.content).unwrap();
    assert_eq!(value["status"], "opened");
    assert!(
        matches!(
            value["view"]["status"].as_str(),
            Some("failed" | "unavailable" | "unknown" | "created_not_placed")
        ),
        "{value}"
    );
    assert!(
        port.inspect(ExecutionQuery::Read(instance.clone()))
            .await
            .is_ok()
    );
    let ExecutionInspection::Instances(instances) =
        port.inspect(ExecutionQuery::Instances).await.unwrap()
    else {
        panic!("missing instance listing");
    };
    assert_eq!(instances, vec![instance.clone()]);
    assert!(matches!(
        execute(&port, &authority, ExecutionIntent::Close { instance }).await,
        ExecutionOutcome::Completed(_)
    ));
}

pub async fn terminal_baseline_contract(
    environment: Arc<dyn ExecutionEnvironment>,
    scope: ExecutionScope,
    host_authority: tickloop_harness_protocol::HostToolAuthority,
    spec: OpenSpec,
) {
    use std::collections::BTreeMap;
    use tickloop_harness_runtime::tick_stream::{
        HostedTickStream, StreamTasks, TickStreamEvent, TickStreamPort, WatchId,
    };
    let chat = scope.chat;
    let (port, authority, opened) = open(environment.as_ref(), scope, spec).await;
    let instance = opened.opened.unwrap().instance;
    wait_screen(&port, &instance, "A").await;
    let stream = HostedTickStream::with_environment(environment, host_authority.clone());
    let first = WatchId::new();
    let second = WatchId::new();
    stream.set_watch_chats(BTreeMap::from([(first, chat.into())]));
    stream.set_watch_authorities(BTreeMap::from([(first, host_authority.clone())]));
    let mut handle = stream.open(&StreamTasks::default());
    handle
        .desired
        .send_replace(BTreeMap::from([(first, instance.clone())]));
    wait_stream(
        &mut handle.events,
        |event| matches!(event, TickStreamEvent::Ready { watch } if *watch == first),
    )
    .await;
    assert!(matches!(
        execute(
            &port,
            &authority,
            ExecutionIntent::SendInput {
                instance: instance.clone(),
                data: b"B\n".to_vec()
            }
        )
        .await,
        ExecutionOutcome::Completed(_)
    ));
    wait_screen(&port, &instance, "B").await;
    wait_stream(&mut handle.events, |event| matches!(event, TickStreamEvent::ScreenChange { audience, is_pending: true, .. } if audience.contains(&first))).await;
    stream.set_watch_chats(BTreeMap::from([
        (first, chat.into()),
        (second, chat.into()),
    ]));
    stream.set_watch_authorities(BTreeMap::from([
        (first, host_authority.clone()),
        (second, host_authority),
    ]));
    handle.desired.send_replace(BTreeMap::from([
        (first, instance.clone()),
        (second, instance.clone()),
    ]));
    wait_stream(
        &mut handle.events,
        |event| matches!(event, TickStreamEvent::Ready { watch } if *watch == second),
    )
    .await;
    assert!(
        stream.sample(second, &instance).await.is_none(),
        "joining content only establishes the new baseline"
    );
    assert!(matches!(
        execute(
            &port,
            &authority,
            ExecutionIntent::SendInput {
                instance: instance.clone(),
                data: b"A\n".to_vec()
            }
        )
        .await,
        ExecutionOutcome::Completed(_)
    ));
    wait_screen(&port, &instance, "A").await;
    wait_stream(&mut handle.events, |event| matches!(event, TickStreamEvent::ScreenChange { audience, is_pending: true, .. } if audience.contains(&second))).await;
    assert!(
        stream.sample(first, &instance).await.is_none(),
        "a screen restored to its baseline is not delivered"
    );
    let changed = stream
        .sample(second, &instance)
        .await
        .expect("the later watch compares against B");
    assert_eq!(changed.data["text"].as_str().unwrap().trim(), "A");
    assert!(stream.sample(second, &instance).await.is_none());
    assert!(matches!(
        execute(&port, &authority, ExecutionIntent::Close { instance }).await,
        ExecutionOutcome::Completed(_)
    ));
}

async fn wait_screen(port: &Arc<dyn ExecutionPort>, instance: &InstanceRef, expected: &str) {
    tokio::time::timeout(std::time::Duration::from_secs(30), async {
        loop {
            let ExecutionInspection::Read(read) = port
                .inspect(ExecutionQuery::Read(instance.clone()))
                .await
                .unwrap()
            else {
                panic!("missing screen");
            };
            if read.state.data["text"]
                .as_str()
                .is_some_and(|text| text.trim() == expected)
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
}

async fn wait_stream(
    events: &mut tokio::sync::mpsc::Receiver<
        tickloop_harness_runtime::tick_stream::TickStreamEvent,
    >,
    accept: impl Fn(&tickloop_harness_runtime::tick_stream::TickStreamEvent) -> bool,
) {
    tokio::time::timeout(std::time::Duration::from_secs(30), async {
        loop {
            if accept(&events.recv().await.expect("stream ended")) {
                break;
            }
        }
    })
    .await
    .unwrap();
}

pub async fn latest_delivery_contract(
    agent: &tickloop_harness_runtime::Agent,
    model: &crate::ScriptedModel,
    environment: &dyn ExecutionEnvironment,
    scope: ExecutionScope,
    spec: OpenSpec,
) {
    use tickloop_harness_protocol::{Content, EventKind, Role};
    let chat = scope.chat;
    let (port, authority, opened) = open(environment, scope, spec).await;
    let instance = opened.opened.unwrap().instance;
    model.enqueue(crate::call(
        "subscribe",
        "TickManage",
        json!({"action":"subscribe","tick_instance_id":instance}),
    ));
    model.enqueue(crate::text("watching"));
    let mut events = agent.subscribe();
    agent
        .submit(chat, "Watch this counter".into())
        .await
        .unwrap();
    loop {
        match crate::next_event(&mut events).await.kind {
            EventKind::Completed { .. } => break,
            EventKind::Failed(error) => panic!("{error}"),
            _ => {}
        }
    }
    assert_eq!(
        model.requests.lock().unwrap().len(),
        2,
        "a baseline does not call the model"
    );
    model.enqueue(crate::text("latest value observed"));
    for _ in 0..2 {
        assert!(matches!(
            execute(
                &port,
                &authority,
                ExecutionIntent::Call {
                    instance: instance.clone(),
                    tool: "increment".into(),
                    args: json!({"amount":1}),
                    revision: "1".into()
                }
            )
            .await,
            ExecutionOutcome::Completed(_)
        ));
    }
    tokio::time::timeout(std::time::Duration::from_secs(80), async {
        loop {
            match events.recv().await.unwrap().kind {
                EventKind::Completed { .. } => break,
                EventKind::Failed(error) => panic!("{error}"),
                _ => {}
            }
        }
    })
    .await
    .expect("high-frequency delivery did not arrive");
    {
        let requests = model.requests.lock().unwrap();
        assert_eq!(requests.len(), 3, "the due batch makes one model call");
        let text = requests[2]
            .messages
            .iter()
            .rev()
            .filter(|message| message.role == Role::User)
            .flat_map(|message| &message.content)
            .find_map(|content| match content {
                Content::Text { text } if text.contains("<tick-delivery>") => Some(text),
                _ => None,
            })
            .expect("missing Tick delivery");
        let start = text.find("<tick-delivery>").unwrap() + "<tick-delivery>".len();
        let end = text.find("</tick-delivery>").unwrap();
        let payload: serde_json::Value = serde_json::from_str(
            &text[start..end]
                .replace("&lt;", "<")
                .replace("&gt;", ">")
                .replace("&amp;", "&"),
        )
        .unwrap();
        assert_eq!(payload["items"][0]["data"]["value"], 2);
        assert_eq!(payload["items"][0]["updates"], 2);
    }
    assert!(matches!(
        execute(&port, &authority, ExecutionIntent::Close { instance }).await,
        ExecutionOutcome::Completed(_)
    ));
}

pub async fn preparation_identity_contract(
    environment: &dyn ExecutionEnvironment,
    scope: ExecutionScope,
    first: ShellSpec,
    second: ShellSpec,
    expected_first: &[u8],
    expected_second: &[u8],
) {
    let authority = ExecutionAuthority::new(scope.chat);
    let port = environment.bind(scope, authority.clone()).unwrap();
    for scenario in 0..3 {
        let operation = tickloop_ids::generate();
        let mut first = first.clone();
        let mut second = second.clone();
        first.operation = operation;
        second.operation = operation;
        first.output = format!("{}.{}", first.output, scenario);
        second.output = first.output.clone();
        let first = port
            .prepare(ExecutionIntent::SpawnShell(first))
            .await
            .unwrap();
        let first_admission = authority.admit(&first).unwrap();
        let second = port
            .prepare(ExecutionIntent::SpawnShell(second))
            .await
            .unwrap();
        let second_admission = authority.admit(&second).unwrap();
        let outcome = match scenario {
            0 => {
                port.execute(first, first_admission, CancellationToken::new())
                    .await
            }
            1 => {
                drop(first);
                port.execute(second, second_admission, CancellationToken::new())
                    .await
            }
            _ => {
                assert!(matches!(
                    port.execute(first, second_admission, CancellationToken::new())
                        .await,
                    ExecutionOutcome::NotDispatched(_)
                ));
                continue;
            }
        };
        let ExecutionOutcome::Completed(started) = outcome else {
            panic!("approved preparation was lost: {outcome:?}");
        };
        let instance = started.shell.unwrap().instance;
        let mut observed = port
            .observe(ObserveRequest {
                instance: instance.clone(),
                after: None,
            })
            .await
            .unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(30), async {
            loop {
                let ExecutionInspection::Shell(status) = port
                    .inspect(ExecutionQuery::ShellStatus(instance.clone()))
                    .await
                    .unwrap()
                else {
                    panic!("missing status");
                };
                if status.state == tickloop_harness_protocol::CommandState::Completed {
                    break;
                }
                observed.updates.changed().await.unwrap();
            }
        })
        .await
        .unwrap();
        let ExecutionInspection::Output(output) = port
            .inspect(ExecutionQuery::ShellOutput {
                instance,
                offset: 0,
                limit: 4096,
            })
            .await
            .unwrap()
        else {
            panic!("missing output");
        };
        assert_eq!(
            output.data,
            if scenario == 0 {
                expected_first
            } else {
                expected_second
            }
        );
    }
}
