use async_trait::async_trait;
use std::sync::atomic::{AtomicBool, Ordering};
use std::{path::PathBuf, sync::Arc};
use tickloop_agent_console::agent_console::{AgentConsole, Destination, Intent};
use tickloop_agent_daemon::EmbeddedAgentAccess;
use tickloop_agent_protocol::{
    AccessError, AccessErrorCode, AgentAccess, AgentCommand, AgentDetailQuery, AgentFilter,
    AgentSummary, CommandEnvelope, CommandReceipt, CommandResult, ConfigurationChange,
    DisplayReceipt, DisplayUpdate, ExecutionAvailability, ExecutionRef, HostLocation, LaunchSpec,
    LoopState, Observation, ObservationRequest, ObservationValue, Page, ReceiptState, UserInput,
    ViewRef,
};
use tickloop_agent_runtime::Agent;
use uuid::Uuid;

async fn create(access: &dyn AgentAccess, epoch: Uuid, directory: PathBuf) -> ExecutionRef {
    let receipt = access
        .execute(CommandEnvelope {
            daemon_epoch: epoch,
            command_id: Uuid::new_v4(),
            command: AgentCommand::Create {
                launch: LaunchSpec::embedded(directory),
                initial: None,
                origin: None,
            },
        })
        .await
        .unwrap();
    let CommandResult::Created { execution, .. } = receipt.into_result().unwrap() else {
        panic!("expected creation")
    };
    execution
}

#[tokio::test]
async fn overview_filters_never_change_new_dispatch_directory_or_visible_execution() {
    let directory = tempfile::tempdir().unwrap();
    let other = tempfile::tempdir().unwrap();
    let runtime = Agent::new_embedded(
        tickloop_agent_test_support::config(),
        Arc::new(tickloop_agent_test_support::ScriptedModel::new(vec![
            tickloop_agent_test_support::text("background reply"),
        ])),
    );
    let access = Arc::new(EmbeddedAgentAccess::new(runtime.clone()));
    let epoch = access.daemon_epoch();
    let mut console = AgentConsole::new(access.clone(), epoch, directory.path().into(), None);
    assert!(console.refresh().await.unwrap().rows.is_empty());
    assert!(console.projection().visible.is_none());
    let original = create(access.as_ref(), epoch, other.path().into()).await;
    console.refresh().await.unwrap();
    console.visible_ready(Some(original));
    console.interpret(Intent::Overview).unwrap();
    console
        .interpret(Intent::Filter(AgentFilter {
            folder: Some(other.path().into()),
            workspace: None,
        }))
        .unwrap();
    let receipt = console
        .dispatch(
            LaunchSpec::embedded(other.path().into()),
            UserInput::Text {
                text: "run separately".into(),
                contexts: vec![],
                paths: vec![],
            },
            None,
        )
        .await
        .unwrap();
    let CommandResult::Created {
        execution,
        snapshot,
        ..
    } = receipt.into_result().unwrap()
    else {
        panic!("created")
    };
    assert_ne!(execution, original);
    assert_eq!(
        snapshot.conversation.folder,
        Some(directory.path().canonicalize().unwrap())
    );
    assert_eq!(console.projection().visible, Some(original));
    assert!(console.projection().overview);
    console
        .interpret(Intent::Filter(AgentFilter::default()))
        .unwrap();
    assert_eq!(console.refresh().await.unwrap().rows.len(), 2);
    runtime.shutdown().await;
}

#[tokio::test]
async fn overview_keeps_each_completed_state_group_contiguous_without_changing_the_target() {
    let directory = tempfile::tempdir().unwrap();
    let runtime = Agent::new_embedded(
        tickloop_agent_test_support::config(),
        Arc::new(tickloop_agent_test_support::ScriptedModel::new(vec![
            tickloop_agent_test_support::text("first done"),
            tickloop_agent_test_support::text("second done"),
        ])),
    );
    let access = Arc::new(EmbeddedAgentAccess::new(runtime.clone()));
    let epoch = access.daemon_epoch();
    let mut executions = Vec::new();
    for _ in 0..3 {
        executions.push(create(access.as_ref(), epoch, directory.path().into()).await);
    }
    executions.sort();
    for execution in [executions[0], executions[2]] {
        access
            .execute(CommandEnvelope {
                daemon_epoch: epoch,
                command_id: Uuid::new_v4(),
                command: AgentCommand::Submit {
                    execution,
                    target_loop: None,
                    input: UserInput::Text {
                        text: "Complete this task.".into(),
                        contexts: vec![],
                        paths: vec![],
                    },
                    origin: None,
                },
            })
            .await
            .unwrap()
            .into_result()
            .unwrap();
    }
    let mut console = AgentConsole::new(access, epoch, directory.path().into(), None);
    console.visible_ready(Some(executions[2]));
    console.interpret(Intent::Overview).unwrap();
    let projection = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let projection = console.refresh().await.unwrap();
            if projection
                .rows
                .iter()
                .filter(|row| row.state == LoopState::Completed)
                .count()
                == 2
            {
                break projection;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(
        projection
            .rows
            .iter()
            .map(|row| row.state.clone())
            .collect::<Vec<_>>(),
        [LoopState::Completed, LoopState::Completed, LoopState::Idle]
    );
    assert!(
        projection.rows[2].has_work == Some(false),
        "the never-used session ranks after finished work"
    );
    assert_eq!(projection.visible, Some(executions[2]));
    assert!(projection.overview);
    runtime.shutdown().await;
}

async fn submit(access: &dyn AgentAccess, epoch: Uuid, execution: ExecutionRef, text: &str) {
    access
        .execute(CommandEnvelope {
            daemon_epoch: epoch,
            command_id: Uuid::new_v4(),
            command: AgentCommand::Submit {
                execution,
                target_loop: None,
                input: UserInput::Text {
                    text: text.into(),
                    contexts: vec![],
                    paths: vec![],
                },
                origin: None,
            },
        })
        .await
        .unwrap()
        .into_result()
        .unwrap();
}

fn order(rows: &[AgentSummary]) -> Vec<ExecutionRef> {
    rows.iter().map(|row| row.execution).collect()
}

#[tokio::test]
async fn overview_completed_families_rank_by_recent_finish() {
    let directory = tempfile::tempdir().unwrap();
    let runtime = Agent::new_embedded(
        tickloop_agent_test_support::config(),
        Arc::new(tickloop_agent_test_support::ScriptedModel::new(vec![
            tickloop_agent_test_support::text("older finish"),
            tickloop_agent_test_support::text("latest finish"),
            tickloop_agent_test_support::text("finished again"),
        ])),
    );
    let access = Arc::new(EmbeddedAgentAccess::new(runtime.clone()));
    let epoch = access.daemon_epoch();
    let mut executions = [
        create(access.as_ref(), epoch, directory.path().into()).await,
        create(access.as_ref(), epoch, directory.path().into()).await,
    ];
    executions.sort();
    // The lower reference finishes first, so reference order and finish order disagree.
    let (older, latest) = (executions[0], executions[1]);
    let mut console = AgentConsole::new(access.clone(), epoch, directory.path().into(), None);
    console.visible_ready(Some(older));
    console.interpret(Intent::Overview).unwrap();
    let completed = |execution: ExecutionRef| {
        move |rows: &[AgentSummary]| {
            rows.iter()
                .any(|row| row.execution == execution && row.state == LoopState::Completed)
        }
    };
    submit(access.as_ref(), epoch, older, "Finish first.").await;
    wait_for_overview(&mut console, completed(older)).await;
    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    submit(access.as_ref(), epoch, latest, "Finish second.").await;
    let rows = wait_for_overview(&mut console, |rows| {
        completed(older)(rows) && completed(latest)(rows)
    })
    .await;
    assert_eq!(
        order(&rows),
        [latest, older],
        "the most recent finish leads Done even though its reference sorts later"
    );
    for _ in 0..3 {
        access
            .observe(ObservationRequest::Agent {
                execution: older,
                detail: AgentDetailQuery::Snapshot,
                after: None,
            })
            .await
            .unwrap();
        access
            .show_view(DisplayUpdate::Displayed {
                view: ViewRef {
                    session_id: Uuid::new_v4(),
                    view_id: Uuid::new_v4(),
                },
                expected_version: 0,
                execution: older,
                host: None,
            })
            .await
            .unwrap();
        assert_eq!(
            order(&console.refresh().await.unwrap().rows),
            [latest, older],
            "reading or displaying an Agent is not work"
        );
    }
    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    submit(access.as_ref(), epoch, older, "Finish again.").await;
    // A queued or running `older` already leads by urgency; wait for both to finish again.
    let rows = wait_for_overview(&mut console, |rows| {
        completed(older)(rows) && completed(latest)(rows) && order(rows) == [older, latest]
    })
    .await;
    let projection = console.projection();
    assert_eq!(projection.visible, Some(older));
    assert!(projection.overview);
    let finished_again = &rows[0];
    assert_eq!(
        finished_again.preview.as_deref(),
        Some("finished again"),
        "the newest reply is the row's recent content"
    );
    let finished_at = finished_again.updated_at.expect("a finish has a time");
    assert!(finished_at >= rows[1].updated_at.expect("a finish has a time"));

    // A captioned image follow-up whose run fails before any reply: the caption is the newest
    // content and the failure, not a reply, moves the time.
    let mut png = std::io::Cursor::new(Vec::new());
    image::DynamicImage::new_rgb8(1, 1)
        .write_to(&mut png, image::ImageFormat::Png)
        .unwrap();
    let image = tickloop_agent_base::image::prepare(&png.into_inner()).unwrap();
    access
        .execute(CommandEnvelope {
            daemon_epoch: epoch,
            command_id: Uuid::new_v4(),
            command: AgentCommand::Submit {
                execution: latest,
                target_loop: None,
                input: UserInput::Text {
                    text: "Describe this chart.".into(),
                    contexts: vec![tickloop_agent_protocol::InputContext::Image(
                        tickloop_agent_protocol::ImageContext {
                            origin: tickloop_agent_protocol::ImageOrigin::Pasted { id: 1 },
                            image,
                        },
                    )],
                    paths: vec![],
                },
                origin: None,
            },
        })
        .await
        .unwrap()
        .into_result()
        .unwrap();
    let rows = wait_for_overview(&mut console, |rows| {
        rows.iter()
            .any(|row| row.execution == latest && row.state == LoopState::Failed)
    })
    .await;
    let failed = rows.iter().find(|row| row.execution == latest).unwrap();
    assert_eq!(failed.preview.as_deref(), Some("Describe this chart."));
    assert!(failed.updated_at.expect("a failure has a time") >= finished_at);
    assert_eq!(order(&rows), [latest, older], "errors lead finished work");
    runtime.shutdown().await;
}

#[tokio::test]
async fn overview_prioritizes_a_waiting_child_family_before_an_independent_running_agent() {
    let directory = tempfile::tempdir().unwrap();
    let runtime = Agent::new_embedded(
        tickloop_agent_test_support::config(),
        Arc::new(tickloop_agent_test_support::ScriptedModel::new(vec![
            tickloop_agent_test_support::call(
                "running",
                "Bash",
                serde_json::json!({
                    "command":"while [ ! -f finish ]; do sleep 0.02; done",
                }),
            ),
            tickloop_agent_test_support::call(
                "delegate",
                "Subagent",
                serde_json::json!({
                    "task":"Ask the user which bounded cadence to use.",
                }),
            ),
            tickloop_agent_test_support::call(
                "child-question",
                "AskUserQuestion",
                tickloop_agent_test_support::single_question("Which cadence?"),
            ),
        ])),
    );
    runtime.install_own_tickhost();
    let access = Arc::new(EmbeddedAgentAccess::new(runtime.clone()));
    let epoch = access.daemon_epoch();
    let mut executions = [
        create(access.as_ref(), epoch, directory.path().into()).await,
        create(access.as_ref(), epoch, directory.path().into()).await,
    ];
    executions.sort();
    let [independent, parent] = executions;
    let mut console = AgentConsole::new(access.clone(), epoch, directory.path().into(), None);
    console.visible_ready(Some(independent));
    console.interpret(Intent::Overview).unwrap();
    for execution in [independent, parent] {
        for command in [
            AgentCommand::Configure {
                provider: None,
                execution,
                change: ConfigurationChange::ExecutionMode {
                    mode: tickloop_agent_protocol::ExecutionMode::FullAccess,
                },
            },
            AgentCommand::Submit {
                execution,
                target_loop: None,
                input: UserInput::Text {
                    text: "Run the bounded task.".into(),
                    contexts: vec![],
                    paths: vec![],
                },
                origin: None,
            },
        ] {
            access
                .execute(CommandEnvelope {
                    daemon_epoch: epoch,
                    command_id: Uuid::new_v4(),
                    command,
                })
                .await
                .unwrap()
                .into_result()
                .unwrap();
        }
        if execution == independent {
            tokio::time::timeout(std::time::Duration::from_secs(5), async {
                loop {
                    let observed = access.observe(ObservationRequest::Agent {
                        execution,
                        detail: AgentDetailQuery::Snapshot,
                        after: None,
                    }).await.unwrap();
                    if matches!(observed.initial, ObservationValue::Agent(snapshot) if snapshot.events.iter().any(|event| matches!(&event.kind, tickloop_agent_protocol::EventKind::ToolStarted { name, .. } if name == "Bash"))) {
                        break;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                }
            }).await.unwrap();
        }
    }
    let projection = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let projection = console.refresh().await.unwrap();
            if projection
                .rows
                .iter()
                .any(|row| row.parent == Some(parent) && row.waiting_questions == 1)
            {
                break projection;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    std::fs::write(directory.path().join("finish"), "finish").unwrap();
    runtime.shutdown().await;
    assert_eq!(
        projection
            .rows
            .iter()
            .filter(|row| row.parent.is_none())
            .map(|row| row.execution)
            .collect::<Vec<_>>(),
        [parent, independent]
    );
    assert_eq!(projection.waiting_permissions, 0);
    assert_eq!(projection.waiting_questions, 1);
    assert_eq!(projection.visible, Some(independent));
    assert_eq!(
        projection
            .rows
            .iter()
            .find(|row| row.execution == parent)
            .unwrap()
            .title,
        "Run the bounded task."
    );
    assert_eq!(
        projection
            .rows
            .iter()
            .find(|row| row.execution == independent)
            .unwrap()
            .title,
        "Run the bounded task."
    );
    let family = console.family(parent);
    assert_eq!(family.len(), 2);
    assert_eq!(
        family
            .iter()
            .filter(|row| row.waiting_questions == 1)
            .count(),
        1
    );
    let child = family
        .iter()
        .find(|row| row.parent == Some(parent))
        .unwrap()
        .execution;
    let navigation = console.interpret(Intent::Open(child)).unwrap().unwrap();
    assert!(matches!(navigation.destination, Destination::Present));
    assert!(console.complete(&navigation));
    assert_eq!(console.projection().visible, Some(child));
    let away = console
        .interpret(Intent::Open(independent))
        .unwrap()
        .unwrap();
    assert!(console.complete(&away));
    console.interpret(Intent::Overview).unwrap();
    let returning = console.interpret(Intent::Open(parent)).unwrap().unwrap();
    assert_eq!(
        returning.target, child,
        "a family restores its last viewed member"
    );
    assert!(console.complete(&returning));
    let other_directory = tempfile::tempdir().unwrap();
    console
        .interpret(Intent::Filter(AgentFilter {
            folder: Some(other_directory.path().into()),
            workspace: None,
        }))
        .unwrap();
    let filtered = console.projection();
    assert!(filtered.rows.is_empty());
    assert_eq!(filtered.visible, Some(child));
    let visible_family = console.family(filtered.visible.unwrap());
    assert_eq!(visible_family.len(), 2);
    assert!(visible_family.iter().any(|row| row.execution == parent));
    assert!(visible_family.iter().any(|row| row.execution == child));
    assert_eq!(
        visible_family
            .iter()
            .find(|row| row.execution == parent)
            .unwrap()
            .title,
        "Run the bounded task."
    );
    let observed = access
        .observe(ObservationRequest::Overview {
            filter: AgentFilter::default(),
            page: Page {
                offset: 0,
                limit: 20,
            },
            after: None,
        })
        .await
        .unwrap();
    let ObservationValue::Overview(canonical) = observed.initial else {
        panic!("actual daemon catalog")
    };
    assert_eq!(
        canonical
            .iter()
            .find(|row| row.execution == parent)
            .unwrap()
            .title,
        "Run the bounded task."
    );
}

#[tokio::test]
async fn late_navigation_and_closed_view_cannot_replace_newer_target() {
    let directory = tempfile::tempdir().unwrap();
    let runtime = Agent::new_embedded(
        tickloop_agent_test_support::config(),
        Arc::new(tickloop_agent_test_support::ScriptedModel::new(vec![])),
    );
    let access = Arc::new(EmbeddedAgentAccess::new(runtime.clone()));
    let epoch = access.daemon_epoch();
    let b = create(access.as_ref(), epoch, directory.path().into()).await;
    let c = create(access.as_ref(), epoch, directory.path().into()).await;
    let mut console = AgentConsole::new(access, epoch, directory.path().into(), None);
    console.refresh().await.unwrap();
    let old = console.interpret(Intent::Open(b)).unwrap().unwrap();
    let new = console.interpret(Intent::Open(c)).unwrap().unwrap();
    assert!(matches!(new.destination, Destination::Present));
    assert!(!console.complete(&old));
    assert!(console.complete(&new));
    assert_eq!(console.projection().visible, Some(c));
    let pending = console.interpret(Intent::Open(b)).unwrap().unwrap();
    console.interpret(Intent::Close).unwrap();
    assert!(!console.complete(&pending));
    assert!(console.interpret(Intent::Overview).is_err());
    runtime.shutdown().await;
}

#[tokio::test]
async fn same_host_multiple_views_require_choice_and_focus_does_not_forge_local_display() {
    let directory = tempfile::tempdir().unwrap();
    let runtime = Agent::new_embedded(
        tickloop_agent_test_support::config(),
        Arc::new(tickloop_agent_test_support::ScriptedModel::new(vec![])),
    );
    let access = Arc::new(EmbeddedAgentAccess::new(runtime.clone()));
    let epoch = access.daemon_epoch();
    let a = create(access.as_ref(), epoch, directory.path().into()).await;
    let b = create(access.as_ref(), epoch, directory.path().into()).await;
    let host = HostLocation {
        host_instance: "fixture-host".into(),
        socket: "/private/tmp/fixture-host".into(),
        workspace: Some("w".into()),
        tab: Some("t".into()),
        pane: Some("p".into()),
        terminal: Some("term".into()),
        client: Some("client".into()),
    };
    let first = ViewRef {
        session_id: Uuid::new_v4(),
        view_id: Uuid::new_v4(),
    };
    let second = ViewRef {
        session_id: Uuid::new_v4(),
        view_id: Uuid::new_v4(),
    };
    for view in [first, second] {
        access
            .show_view(DisplayUpdate::Displayed {
                view,
                expected_version: 0,
                execution: b,
                host: Some(host.clone()),
            })
            .await
            .unwrap();
    }
    let mut console = AgentConsole::new(access.clone(), epoch, directory.path().into(), Some(host));
    console.visible_ready(Some(a));
    console.refresh().await.unwrap();
    let choose = console.interpret(Intent::Open(b)).unwrap().unwrap();
    assert!(matches!(choose.destination, Destination::Choose(_)));
    let focus = console
        .interpret(Intent::ChooseLocation {
            generation: choose.generation,
            view: second,
        })
        .unwrap()
        .unwrap();
    assert!(console.revalidate(&focus).await.unwrap());
    assert!(console.complete(&focus));
    assert_eq!(console.projection().visible, Some(a));
    let recent = console.interpret(Intent::Open(b)).unwrap().unwrap();
    assert!(
        matches!(recent.destination,Destination::Focus(ref destination) if destination.view==second)
    );
    access
        .show_view(DisplayUpdate::Left {
            view: second,
            version: 1,
        })
        .await
        .unwrap();
    assert!(!console.revalidate(&recent).await.unwrap());
    runtime.shutdown().await;
}

#[tokio::test]
async fn an_unavailable_pane_falls_back_in_place_until_a_fresh_display_is_registered() {
    let directory = tempfile::tempdir().unwrap();
    let runtime = Agent::new_embedded(
        tickloop_agent_test_support::config(),
        Arc::new(tickloop_agent_test_support::ScriptedModel::new(vec![])),
    );
    let access = Arc::new(EmbeddedAgentAccess::new(runtime.clone()));
    let epoch = access.daemon_epoch();
    let a = create(access.as_ref(), epoch, directory.path().into()).await;
    let b = create(access.as_ref(), epoch, directory.path().into()).await;
    let host = HostLocation {
        host_instance: "same-host-instance".into(),
        socket: "/tmp/same-host-instance.sock".into(),
        workspace: Some("workspace".into()),
        tab: Some("tab".into()),
        pane: Some("pane".into()),
        terminal: Some("terminal".into()),
        client: None,
    };
    let view = ViewRef {
        session_id: Uuid::new_v4(),
        view_id: Uuid::new_v4(),
    };
    access
        .show_view(DisplayUpdate::Displayed {
            view,
            expected_version: 0,
            execution: b,
            host: Some(host.clone()),
        })
        .await
        .unwrap();
    let mut console = AgentConsole::new(
        access.clone(),
        epoch,
        directory.path().into(),
        Some(host.clone()),
    );
    console.visible_ready(Some(a));
    console.refresh().await.unwrap();
    let old_focus = console.interpret(Intent::Open(b)).unwrap().unwrap();
    assert!(matches!(old_focus.destination, Destination::Focus(_)));
    assert!(console.location_unavailable(&old_focus));
    console.refresh().await.unwrap();
    let fallback = console.interpret(Intent::Open(b)).unwrap().unwrap();
    assert!(matches!(fallback.destination, Destination::Present));
    assert!(!console.location_unavailable(&old_focus));
    assert!(console.complete(&fallback));
    assert_eq!(console.projection().visible, Some(b));
    assert_eq!(console.projection().rows.len(), 2);
    access
        .show_view(DisplayUpdate::Displayed {
            view,
            expected_version: 1,
            execution: b,
            host: Some(host.clone()),
        })
        .await
        .unwrap();
    console.visible_ready(Some(a));
    console.refresh().await.unwrap();
    let refreshed = console.interpret(Intent::Open(b)).unwrap().unwrap();
    assert!(matches!(refreshed.destination, Destination::Focus(ref target) if target.version==2));
    let mut other_instance = host.clone();
    other_instance.socket = "/tmp/other-instance.sock".into();
    access
        .show_view(DisplayUpdate::Displayed {
            view,
            expected_version: 2,
            execution: b,
            host: Some(other_instance),
        })
        .await
        .unwrap();
    assert!(!console.revalidate(&refreshed).await.unwrap());
    let other = console.interpret(Intent::Open(b)).unwrap().unwrap();
    assert!(matches!(other.destination, Destination::Present));
    access
        .show_view(DisplayUpdate::Scene {
            view,
            expected_version: 3,
            host: Some(host),
        })
        .await
        .unwrap();
    console.refresh().await.unwrap();
    assert!(
        console
            .projection()
            .rows
            .iter()
            .find(|row| row.execution == b)
            .unwrap()
            .views
            .is_empty()
    );
    runtime.shutdown().await;
}

struct LostCreationReply {
    actual: EmbeddedAgentAccess,
    hide_receipt: AtomicBool,
}

#[async_trait]
impl AgentAccess for LostCreationReply {
    async fn observe(&self, query: ObservationRequest) -> Result<Observation, AccessError> {
        let hide = matches!(query, ObservationRequest::Receipt { .. })
            && self.hide_receipt.load(Ordering::SeqCst);
        let mut observation = self.actual.observe(query).await?;
        if hide {
            observation.initial = ObservationValue::Receipt(None);
        }
        Ok(observation)
    }

    async fn execute(&self, command: CommandEnvelope) -> Result<CommandReceipt, AccessError> {
        let receipt = self.actual.execute(command).await?;
        if self.hide_receipt.load(Ordering::SeqCst) {
            Err(AccessError::new(
                AccessErrorCode::OutcomeUnknown,
                "the creation reply was lost",
            ))
        } else {
            Ok(receipt)
        }
    }

    async fn show_view(&self, update: DisplayUpdate) -> Result<DisplayReceipt, AccessError> {
        self.actual.show_view(update).await
    }
}

#[tokio::test]
async fn unknown_creation_requires_receipt_or_explicit_acknowledgement_and_never_replays() {
    let directory = tempfile::tempdir().unwrap();
    let runtime = Agent::new_embedded(
        tickloop_agent_test_support::config(),
        Arc::new(tickloop_agent_test_support::ScriptedModel::new(vec![
            tickloop_agent_test_support::text("first"),
            tickloop_agent_test_support::text("second"),
        ])),
    );
    let actual = EmbeddedAgentAccess::new(runtime.clone());
    let epoch = actual.daemon_epoch();
    let access = Arc::new(LostCreationReply {
        actual,
        hide_receipt: AtomicBool::new(true),
    });
    let mut console = AgentConsole::new(access.clone(), epoch, directory.path().into(), None);
    console.interpret(Intent::Overview).unwrap();
    let launch = || LaunchSpec::embedded(directory.path().into());
    let input = || UserInput::Text {
        text: "work independently".into(),
        contexts: vec![],
        paths: vec![],
    };
    let unknown = console.dispatch(launch(), input(), None).await.unwrap_err();
    assert_eq!(unknown.code, AccessErrorCode::OutcomeUnknown);
    let command_id = console.projection().unconfirmed_dispatch.unwrap();
    assert_eq!(console.refresh().await.unwrap().rows.len(), 1);
    assert_eq!(
        console
            .dispatch(launch(), input(), None)
            .await
            .unwrap_err()
            .code,
        AccessErrorCode::OutcomeUnknown
    );
    assert!(
        console
            .interpret(Intent::AcknowledgeUnconfirmedDispatch(Uuid::new_v4()))
            .is_err()
    );
    console
        .interpret(Intent::AcknowledgeUnconfirmedDispatch(command_id))
        .unwrap();
    assert!(console.projection().unconfirmed_dispatch.is_none());
    assert_eq!(console.refresh().await.unwrap().rows.len(), 1);
    access.hide_receipt.store(false, Ordering::SeqCst);
    let receipt = access
        .observe(ObservationRequest::Receipt { epoch, command_id })
        .await
        .unwrap();
    assert!(
        matches!(receipt.initial,ObservationValue::Receipt(Some(receipt)) if matches!(receipt.state,ReceiptState::Completed))
    );
    console
        .dispatch(launch(), input(), None)
        .await
        .unwrap()
        .into_result()
        .unwrap();
    assert_eq!(console.refresh().await.unwrap().rows.len(), 2);
    assert!(console.projection().overview);
    runtime.shutdown().await;
}

fn user_shell(command: &str) -> UserInput {
    UserInput::Shell {
        command: command.into(),
        background: true,
    }
}

async fn create_with(
    access: &dyn AgentAccess,
    epoch: Uuid,
    directory: PathBuf,
    initial: UserInput,
) -> ExecutionRef {
    let receipt = access
        .execute(CommandEnvelope {
            daemon_epoch: epoch,
            command_id: Uuid::new_v4(),
            command: AgentCommand::Create {
                launch: LaunchSpec::embedded(directory),
                initial: Some(initial),
                origin: None,
            },
        })
        .await
        .unwrap();
    let CommandResult::Created { execution, .. } = receipt.into_result().unwrap() else {
        panic!("expected creation")
    };
    execution
}

#[tokio::test]
async fn an_accepted_dispatch_names_its_actual_execution_without_moving_filter_target_or_cwd() {
    let launch = tempfile::tempdir().unwrap();
    let monitored = tempfile::tempdir().unwrap();
    let launch_directory = launch.path().canonicalize().unwrap();
    let runtime = Agent::new_embedded(
        tickloop_agent_test_support::config(),
        Arc::new(tickloop_agent_test_support::ScriptedModel::new(vec![])),
    );
    let access = Arc::new(EmbeddedAgentAccess::new(runtime.clone()));
    let epoch = access.daemon_epoch();
    let bravo = create_with(
        access.as_ref(),
        epoch,
        monitored.path().into(),
        user_shell("printf bravo"),
    )
    .await;
    let older = create_with(
        access.as_ref(),
        epoch,
        launch_directory.clone(),
        user_shell("pwd"),
    )
    .await;
    let mut console = AgentConsole::new(access.clone(), epoch, launch_directory.clone(), None);
    console.refresh().await.unwrap();
    console.visible_ready(Some(bravo));
    console.interpret(Intent::Overview).unwrap();
    let monitor = AgentFilter {
        folder: Some(monitored.path().canonicalize().unwrap()),
        workspace: None,
    };
    console.interpret(Intent::Filter(monitor.clone())).unwrap();
    assert!(console.projection().created.is_none());
    let receipt = console
        .dispatch(
            LaunchSpec::embedded(monitored.path().into()),
            user_shell("pwd"),
            None,
        )
        .await
        .unwrap();
    let CommandResult::Created { execution, .. } = receipt.into_result().unwrap() else {
        panic!("expected creation")
    };
    assert_ne!(execution, older);
    let projection = console.refresh().await.unwrap();
    assert_eq!(projection.created, Some(execution));
    assert_eq!(projection.visible, Some(bravo));
    assert!(projection.overview);
    assert_eq!(projection.dispatch_directory, launch_directory);
    assert_eq!(
        projection
            .rows
            .iter()
            .map(|row| row.execution)
            .collect::<Vec<_>>(),
        [bravo],
        "the monitored filter keeps hiding the new Agent"
    );
    console
        .interpret(Intent::Filter(AgentFilter::default()))
        .unwrap();
    let projection = console.projection();
    let titles = |target: ExecutionRef| {
        projection
            .rows
            .iter()
            .find(|row| row.execution == target)
            .map(|row| (row.title.clone(), row.folder.clone()))
            .unwrap()
    };
    assert_eq!(
        titles(execution),
        ("!pwd".into(), Some(launch_directory.clone()))
    );
    assert_eq!(titles(older), titles(execution));
    assert_eq!(projection.created, Some(execution));
    console.interpret(Intent::Filter(monitor)).unwrap();
    let next = console
        .dispatch(
            LaunchSpec::embedded(launch_directory.clone()),
            user_shell("printf next"),
            None,
        )
        .await
        .unwrap();
    assert_eq!(console.projection().created, next.target);
    assert_ne!(next.target, Some(execution));
    console.refresh().await.unwrap();
    let opened = console.interpret(Intent::Open(bravo)).unwrap().unwrap();
    assert!(console.complete(&opened));
    assert_eq!(
        console.projection().created,
        next.target,
        "opening another Agent keeps the creation cue"
    );
    console.interpret(Intent::Overview).unwrap();
    let opened = console
        .interpret(Intent::Open(next.target.unwrap()))
        .unwrap()
        .unwrap();
    assert!(console.complete(&opened));
    assert!(console.projection().created.is_none());
    console
        .dispatch(
            LaunchSpec::embedded(launch_directory.clone()),
            user_shell("printf last"),
            None,
        )
        .await
        .unwrap();
    assert!(console.projection().created.is_some());
    console.interpret(Intent::Back).unwrap();
    assert!(console.projection().created.is_none());
    runtime.shutdown().await;
}

#[tokio::test]
async fn only_a_resolved_creation_receipt_produces_a_created_execution() {
    let directory = tempfile::tempdir().unwrap();
    let runtime = Agent::new_embedded(
        tickloop_agent_test_support::config(),
        Arc::new(tickloop_agent_test_support::ScriptedModel::new(vec![])),
    );
    let actual = EmbeddedAgentAccess::new(runtime.clone());
    let epoch = actual.daemon_epoch();
    let access = Arc::new(LostCreationReply {
        actual,
        hide_receipt: AtomicBool::new(false),
    });
    let mut console = AgentConsole::new(access.clone(), epoch, directory.path().into(), None);
    console.interpret(Intent::Overview).unwrap();
    let launch = || LaunchSpec::embedded(directory.path().into());
    let first = console
        .dispatch(launch(), user_shell("printf first"), None)
        .await
        .unwrap();
    assert!(first.target.is_some());
    assert_eq!(console.projection().created, first.target);
    access.hide_receipt.store(true, Ordering::SeqCst);
    let unknown = console
        .dispatch(launch(), user_shell("printf second"), None)
        .await
        .unwrap_err();
    assert_eq!(unknown.code, AccessErrorCode::OutcomeUnknown);
    assert!(
        console.projection().created.is_none(),
        "an unknown outcome must not keep or claim a created Agent"
    );
    assert_eq!(console.refresh().await.unwrap().rows.len(), 2);
    assert!(console.reconcile_dispatch().await.unwrap().is_none());
    assert!(console.projection().created.is_none());
    access.hide_receipt.store(false, Ordering::SeqCst);
    let resolved = console.reconcile_dispatch().await.unwrap().unwrap();
    assert!(matches!(resolved.state, ReceiptState::Completed));
    assert!(
        resolved
            .target
            .is_some_and(|target| first.target != Some(target))
    );
    assert_eq!(console.projection().created, resolved.target);
    assert!(console.projection().unconfirmed_dispatch.is_none());
    access.hide_receipt.store(true, Ordering::SeqCst);
    console
        .dispatch(launch(), user_shell("printf third"), None)
        .await
        .unwrap_err();
    let command_id = console.projection().unconfirmed_dispatch.unwrap();
    console
        .interpret(Intent::AcknowledgeUnconfirmedDispatch(command_id))
        .unwrap();
    assert!(console.projection().created.is_none());
    runtime.shutdown().await;
}

async fn wait_for_overview(
    console: &mut AgentConsole,
    ready: impl Fn(&[AgentSummary]) -> bool,
) -> Vec<AgentSummary> {
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let projection = console.refresh().await.unwrap();
            if ready(&projection.rows) {
                break projection.rows;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap()
}

#[tokio::test]
async fn overview_rows_tell_configured_empty_sessions_from_agents_that_worked() {
    let directory = tempfile::tempdir().unwrap();
    let runtime = Agent::new_embedded(
        tickloop_agent_test_support::config(),
        Arc::new(tickloop_agent_test_support::ScriptedModel::new(vec![
            tickloop_agent_test_support::text("done"),
        ])),
    );
    let access = Arc::new(EmbeddedAgentAccess::new(runtime.clone()));
    let epoch = access.daemon_epoch();
    let configured = create(access.as_ref(), epoch, directory.path().into()).await;
    access
        .execute(CommandEnvelope {
            daemon_epoch: epoch,
            command_id: Uuid::new_v4(),
            command: AgentCommand::Configure {
                provider: None,
                execution: configured,
                change: ConfigurationChange::ExecutionMode {
                    mode: tickloop_agent_protocol::ExecutionMode::FullAccess,
                },
            },
        })
        .await
        .unwrap()
        .into_result()
        .unwrap();
    let shell_only = create_with(
        access.as_ref(),
        epoch,
        directory.path().into(),
        user_shell("printf shell"),
    )
    .await;
    let prompted = create_with(
        access.as_ref(),
        epoch,
        directory.path().into(),
        UserInput::Text {
            text: "Finish once.".into(),
            contexts: vec![],
            paths: vec![],
        },
    )
    .await;
    let mut console = AgentConsole::new(access, epoch, directory.path().into(), None);
    let rows = wait_for_overview(&mut console, |rows| {
        rows.iter()
            .any(|row| row.execution == prompted && row.state == LoopState::Completed)
    })
    .await;
    let row = |execution| rows.iter().find(|row| row.execution == execution).unwrap();
    assert_eq!(row(configured).has_work, Some(false));
    assert_eq!(row(configured).state, LoopState::Idle);
    assert_eq!(row(shell_only).has_work, Some(true));
    assert_eq!(row(prompted).has_work, Some(true));
    assert_eq!(
        row(configured).updated_at,
        None,
        "configuration alone is not work and has no work time"
    );
    assert!(
        row(shell_only).updated_at.is_some(),
        "a Shell state change is work"
    );
    assert!(row(prompted).updated_at.is_some(), "a finish has a time");
    assert_eq!(
        rows.last().map(|row| row.execution),
        Some(configured),
        "a session that never worked ranks after every Agent that did"
    );
    runtime.shutdown().await;
}

#[tokio::test]
async fn an_armed_waiting_goal_is_not_masked_by_a_later_completed_loop() {
    let directory = tempfile::tempdir().unwrap();
    let runtime = Agent::new_embedded(
        tickloop_agent_test_support::config(),
        Arc::new(tickloop_agent_test_support::ScriptedModel::new(vec![
            tickloop_agent_test_support::text("watching"),
            tickloop_agent_test_support::text("answered"),
        ])),
    );
    let access = Arc::new(EmbeddedAgentAccess::new(runtime.clone()));
    let epoch = access.daemon_epoch();
    let execution = create(access.as_ref(), epoch, directory.path().into()).await;
    let mut console = AgentConsole::new(access.clone(), epoch, directory.path().into(), None);
    let chat = console.refresh().await.unwrap().rows[0].chat_id;
    let watching = runtime
        .submit(chat, None, "Watch the notes when asked".into())
        .await
        .unwrap();
    runtime
        .configure_trigger(
            chat,
            watching.loop_id,
            Some(tickloop_agent_protocol::Trigger {
                id: Uuid::new_v4(),
                prompt: "Check the notes".into(),
                prompt_source: tickloop_agent_protocol::TriggerPromptSource::Explicit,
                schedule: None,
                armed_at_ms: None,
                last_fired_at_ms: None,
                expires_at_ms: None,
                accepts_events: true,
                armed: true,
                subscriptions: Vec::new(),
            }),
        )
        .await
        .unwrap();
    wait_for_overview(&mut console, |rows| {
        rows[0].state == LoopState::WaitingTrigger
    })
    .await;
    runtime
        .submit(chat, None, "Answer one unrelated question".into())
        .await
        .unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let snapshot = runtime.snapshot(chat).await.unwrap();
            if snapshot.loops.len() == 2 && snapshot.loops[1].state == LoopState::Completed {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    let rows = console.refresh().await.unwrap().rows;
    assert_eq!(rows[0].execution, execution);
    assert_eq!(rows[0].state, LoopState::WaitingTrigger);
    runtime.shutdown().await;
}

/// Reports chosen executions the way the daemon reports a lost worker's last directory entries.
struct LostWorkerDirectory {
    actual: EmbeddedAgentAccess,
    lost: ExecutionRef,
}

#[async_trait]
impl AgentAccess for LostWorkerDirectory {
    async fn observe(&self, query: ObservationRequest) -> Result<Observation, AccessError> {
        let mut observation = self.actual.observe(query).await?;
        if let ObservationValue::Overview(rows) = &mut observation.initial {
            for row in rows.iter_mut().filter(|row| row.execution == self.lost) {
                row.availability = ExecutionAvailability::Lost;
                row.views.clear();
                row.waiting_permissions = 0;
                row.waiting_questions = 0;
            }
        }
        Ok(observation)
    }

    async fn execute(&self, command: CommandEnvelope) -> Result<CommandReceipt, AccessError> {
        self.actual.execute(command).await
    }

    async fn show_view(&self, update: DisplayUpdate) -> Result<DisplayReceipt, AccessError> {
        self.actual.show_view(update).await
    }
}

#[tokio::test]
async fn a_lost_execution_stays_an_error_even_before_it_ever_worked() {
    let directory = tempfile::tempdir().unwrap();
    let runtime = Agent::new_embedded(
        tickloop_agent_test_support::config(),
        Arc::new(tickloop_agent_test_support::ScriptedModel::new(vec![
            tickloop_agent_test_support::text("done"),
        ])),
    );
    let actual = EmbeddedAgentAccess::new(runtime.clone());
    let epoch = actual.daemon_epoch();
    let configured = create(&actual, epoch, directory.path().into()).await;
    let finished = create_with(
        &actual,
        epoch,
        directory.path().into(),
        UserInput::Text {
            text: "Finish once.".into(),
            contexts: vec![],
            paths: vec![],
        },
    )
    .await;
    let access = Arc::new(LostWorkerDirectory {
        actual,
        lost: configured,
    });
    let mut console = AgentConsole::new(access, epoch, directory.path().into(), None);
    let rows = wait_for_overview(&mut console, |rows| {
        rows.iter()
            .any(|row| row.execution == finished && row.state == LoopState::Completed)
    })
    .await;
    let lost = rows.iter().find(|row| row.execution == configured).unwrap();
    assert_eq!(lost.has_work, Some(false));
    assert_eq!(lost.availability, ExecutionAvailability::Lost);
    assert_eq!(
        tickloop_agent_console::agent_console::AttentionGroup::of(lost),
        tickloop_agent_console::agent_console::AttentionGroup::Errors
    );
    assert_eq!(
        rows.iter().map(|row| row.execution).collect::<Vec<_>>(),
        [configured, finished],
        "a lost execution ranks with errors before finished work"
    );
    runtime.shutdown().await;
}

#[tokio::test]
async fn an_active_user_shell_keeps_an_agent_working_after_its_loops_finish() {
    let directory = tempfile::tempdir().unwrap();
    let runtime = Agent::new_embedded(
        tickloop_agent_test_support::config(),
        Arc::new(tickloop_agent_test_support::ScriptedModel::new(vec![
            tickloop_agent_test_support::text("done"),
        ])),
    );
    runtime.install_own_tickhost();
    let access = Arc::new(EmbeddedAgentAccess::new(runtime.clone()));
    let epoch = access.daemon_epoch();
    let hold = |name: &str| {
        format!(
            "for _ in $(seq 1 500); do [ -f {} ] && break; sleep 0.02; done",
            directory.path().join(name).display()
        )
    };
    let finished = create_with(
        access.as_ref(),
        epoch,
        directory.path().into(),
        UserInput::Text {
            text: "Finish once.".into(),
            contexts: vec![],
            paths: vec![],
        },
    )
    .await;
    let mut console = AgentConsole::new(access.clone(), epoch, directory.path().into(), None);
    wait_for_overview(&mut console, |rows| {
        rows.iter()
            .any(|row| row.execution == finished && row.state == LoopState::Completed)
    })
    .await;
    access
        .execute(CommandEnvelope {
            daemon_epoch: epoch,
            command_id: Uuid::new_v4(),
            command: AgentCommand::Submit {
                execution: finished,
                target_loop: None,
                input: UserInput::Shell {
                    command: hold("release-foreground"),
                    background: false,
                },
                origin: None,
            },
        })
        .await
        .unwrap()
        .into_result()
        .unwrap();
    let background = create_with(
        access.as_ref(),
        epoch,
        directory.path().into(),
        user_shell(&hold("release-background")),
    )
    .await;
    let failed = create_with(
        access.as_ref(),
        epoch,
        directory.path().into(),
        user_shell("exit 3"),
    )
    .await;
    let rows = wait_for_overview(&mut console, |rows| {
        let state = |execution| {
            rows.iter()
                .find(|row| row.execution == execution)
                .map(|row| row.state.clone())
        };
        state(finished) == Some(LoopState::Running)
            && state(background) == Some(LoopState::Running)
            && state(failed) == Some(LoopState::Failed)
    })
    .await;
    let group = |execution| {
        tickloop_agent_console::agent_console::AttentionGroup::of(
            rows.iter().find(|row| row.execution == execution).unwrap(),
        )
    };
    assert_eq!(
        group(finished),
        tickloop_agent_console::agent_console::AttentionGroup::Working
    );
    assert_eq!(
        group(background),
        tickloop_agent_console::agent_console::AttentionGroup::Working
    );
    assert_eq!(
        group(failed),
        tickloop_agent_console::agent_console::AttentionGroup::Errors
    );
    for name in ["release-foreground", "release-background"] {
        std::fs::write(directory.path().join(name), "done").unwrap();
    }
    let rows = wait_for_overview(&mut console, |rows| {
        rows.iter().all(|row| row.state != LoopState::Running)
    })
    .await;
    let state = |execution| {
        rows.iter()
            .find(|row| row.execution == execution)
            .unwrap()
            .state
            .clone()
    };
    assert_eq!(state(finished), LoopState::Completed);
    assert_eq!(state(background), LoopState::Idle);
    runtime.shutdown().await;
}

/// Bounded cost observation for the overview catalog, which snapshots every Agent on refresh.
/// It prints timings only; it is not a performance assertion.
#[tokio::test]
#[ignore = "bounded cost observation; run explicitly and record the printed timings"]
async fn overview_refresh_cost_with_many_long_histories() {
    const AGENTS: usize = 40;
    const REFRESHES: u32 = 10;
    for (turns, history_bytes) in [(1, 1_000), (1, 200_000), (40, 1_000)] {
        let directory = tempfile::tempdir().unwrap();
        let runtime = Agent::new_embedded(
            tickloop_agent_test_support::config(),
            Arc::new(tickloop_agent_test_support::ScriptedModel::new(
                (0..AGENTS * turns)
                    .map(|_| tickloop_agent_test_support::text("done"))
                    .collect(),
            )),
        );
        let access = Arc::new(EmbeddedAgentAccess::new(runtime.clone()));
        let epoch = access.daemon_epoch();
        let input = || UserInput::Text {
            text: "x".repeat(history_bytes),
            contexts: vec![],
            paths: vec![],
        };
        let mut console = AgentConsole::new(access.clone(), epoch, directory.path().into(), None);
        for _ in 0..AGENTS {
            let execution =
                create_with(access.as_ref(), epoch, directory.path().into(), input()).await;
            for _ in 1..turns {
                wait_for_overview(&mut console, |rows| {
                    rows.iter()
                        .any(|row| row.execution == execution && row.state == LoopState::Completed)
                })
                .await;
                let chat = console
                    .refresh()
                    .await
                    .unwrap()
                    .rows
                    .into_iter()
                    .find(|row| row.execution == execution)
                    .unwrap()
                    .chat_id;
                runtime
                    .submit(chat, None, "x".repeat(history_bytes))
                    .await
                    .unwrap();
            }
        }
        wait_for_overview(&mut console, |rows| {
            rows.len() == AGENTS && rows.iter().all(|row| row.state == LoopState::Completed)
        })
        .await;
        let started = std::time::Instant::now();
        for _ in 0..REFRESHES {
            console.refresh().await.unwrap();
        }
        eprintln!(
            "OVERVIEW-REFRESH-COST agents={AGENTS} turns={turns} bytes_per_input={history_bytes} mean_refresh_ms={:.2}",
            started.elapsed().as_secs_f64() * 1000.0 / f64::from(REFRESHES)
        );
        runtime.shutdown().await;
    }
}
