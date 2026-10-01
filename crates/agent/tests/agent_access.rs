mod support;

use std::{path::PathBuf, sync::Arc, time::Duration};

use tickloop_agent::{
    agent_access::{
        AgentAccess, AgentCommand, AgentDetailQuery, CommandEnvelope, CommandResult,
        EmbeddedAgentAccess, LaunchSpec, ObservationRequest, ObservationValue, RequestKind,
        RequestRef, UserInput,
    },
    runtime::{Agent, QuestionAnswer},
};
use uuid::Uuid;

#[tokio::test]
async fn embedded_caller_can_answer_the_original_request_and_observe_the_result() {
    let directory = tempfile::tempdir().unwrap();
    let model = Arc::new(support::ScriptedModel::new(vec![
        support::call(
            "question",
            "AskUserQuestion",
            support::single_question("How often?"),
        ),
        support::text("The weekly report is ready."),
    ]));
    let runtime =
        Agent::open_embedded(directory.path().join("state"), support::config(), model).unwrap();
    let access = EmbeddedAgentAccess::new(runtime.clone());
    let created = access
        .execute(CommandEnvelope {
            owner_epoch: access.owner_epoch(),
            command_id: Uuid::new_v4(),
            command: AgentCommand::Create {
                launch: LaunchSpec::embedded(PathBuf::from(directory.path())),
                initial: Some(UserInput::Text {
                    text: "Prepare a report".into(),
                    contexts: vec![],
                    paths: vec![],
                }),
                origin: None,
            },
        })
        .await
        .unwrap();
    let Some(CommandResult::Created { execution, .. }) = created.result else {
        panic!("Create must return the captured execution: {created:?}");
    };
    let mut observation = access
        .observe(ObservationRequest::Agent {
            execution,
            detail: AgentDetailQuery::Snapshot,
            after: None,
        })
        .await
        .unwrap();
    let request = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let current = access
                .observe(ObservationRequest::Agent {
                    execution,
                    detail: AgentDetailQuery::Snapshot,
                    after: None,
                })
                .await
                .unwrap();
            if let ObservationValue::Agent(snapshot) = current.initial
                && let Some(question) = snapshot.questions.first()
            {
                break RequestRef {
                    execution,
                    request_id: question.id,
                    kind: RequestKind::Question,
                    version: 1,
                };
            }
            observation.recv().await.unwrap();
        }
    })
    .await
    .unwrap();
    access
        .execute(CommandEnvelope {
            owner_epoch: access.owner_epoch(),
            command_id: Uuid::new_v4(),
            command: AgentCommand::AnswerQuestion {
                request,
                answers: vec![QuestionAnswer::option("Weekly")],
            },
        })
        .await
        .unwrap();
    drop(observation);
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let current = access
                .observe(ObservationRequest::Agent {
                    execution,
                    detail: AgentDetailQuery::Snapshot,
                    after: None,
                })
                .await
                .unwrap();
            if let ObservationValue::Agent(snapshot) = current.initial
                && snapshot
                    .conversation
                    .messages
                    .iter()
                    .any(|message| message.content.iter().any(|block| matches!(block, tickloop_agent::model::Content::Text { text } if text.contains("The weekly report is ready."))))
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    runtime.shutdown().await;
}

async fn live(
    access: &dyn AgentAccess,
    execution: tickloop_agent::agent_access::ExecutionRef,
) -> tickloop_agent::agent_access::LiveChatSnapshot {
    match access
        .observe(ObservationRequest::Agent {
            execution,
            detail: AgentDetailQuery::Snapshot,
            after: None,
        })
        .await
        .unwrap()
        .initial
    {
        ObservationValue::Agent(snapshot) => *snapshot,
        other => panic!("unexpected snapshot: {other:?}"),
    }
}

#[tokio::test]
async fn failed_question_validation_advances_the_version_and_only_one_answer_wins() {
    use tickloop_agent::agent_access::{AccessErrorCode, ReceiptState};
    let temporary = tempfile::tempdir().unwrap();
    let model = Arc::new(support::ScriptedModel::new(vec![
        support::call("q", "AskUserQuestion", support::single_question("Cadence?")),
        support::text("Weekly selected."),
    ]));
    let runtime = Agent::new_embedded(support::config(), model.clone());
    let access = EmbeddedAgentAccess::new(runtime.clone());
    let created = access
        .execute(CommandEnvelope {
            owner_epoch: access.owner_epoch(),
            command_id: Uuid::new_v4(),
            command: AgentCommand::Create {
                launch: LaunchSpec::embedded(temporary.path().to_path_buf()),
                initial: Some(UserInput::Text {
                    text: "Pick cadence".into(),
                    contexts: vec![],
                    paths: vec![],
                }),
                origin: None,
            },
        })
        .await
        .unwrap()
        .into_result()
        .unwrap();
    let CommandResult::Created { execution, .. } = created else {
        panic!("expected Create");
    };
    let request = tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if let Some(request) = live(&access, execution).await.request_refs.first().copied() {
                break request;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let failed = access
        .execute(CommandEnvelope {
            owner_epoch: access.owner_epoch(),
            command_id: Uuid::new_v4(),
            command: AgentCommand::AnswerQuestion {
                request,
                answers: vec![],
            },
        })
        .await
        .unwrap();
    assert!(matches!(failed.state, ReceiptState::Failed));
    let current = live(&access, execution).await;
    let updated = current.request_refs[0];
    assert_eq!(updated.request_id, request.request_id);
    assert_eq!(updated.version, request.version + 1);
    let wire: tickloop_agent::agent_access::LiveChatSnapshot =
        serde_json::from_slice(&serde_json::to_vec(&current).unwrap()).unwrap();
    assert_eq!(wire.model.name, "fixture");
    assert_eq!(wire.clone().into_snapshot().model.name, "fixture");
    assert_eq!(
        wire.clone().into_snapshot().questions[0].id,
        request.request_id
    );
    assert_eq!(wire.request_refs[0], updated);
    let stale = access
        .execute(CommandEnvelope {
            owner_epoch: access.owner_epoch(),
            command_id: Uuid::new_v4(),
            command: AgentCommand::AnswerQuestion {
                request,
                answers: vec![QuestionAnswer::option("Monthly")],
            },
        })
        .await
        .unwrap();
    assert_eq!(stale.error.unwrap().code, AccessErrorCode::VersionConflict);
    let command = |label: &str| CommandEnvelope {
        owner_epoch: access.owner_epoch(),
        command_id: Uuid::new_v4(),
        command: AgentCommand::AnswerQuestion {
            request: updated,
            answers: vec![QuestionAnswer::option(label)],
        },
    };
    let (first, second) = tokio::join!(
        access.execute(command("Weekly")),
        access.execute(command("Monthly"))
    );
    let outcomes = [first.unwrap(), second.unwrap()];
    assert_eq!(
        outcomes
            .iter()
            .filter(|receipt| matches!(receipt.state, ReceiptState::Completed))
            .count(),
        1
    );
    assert!(
        outcomes
            .iter()
            .filter_map(|receipt| receipt.error.as_ref())
            .all(|error| matches!(
                error.code,
                AccessErrorCode::Responding | AccessErrorCode::AlreadyResolved
            ))
    );
    tokio::time::timeout(Duration::from_secs(3), async { loop {
        if live(&access, execution).await.conversation.messages.iter().any(|message| message.content.iter().any(|content| matches!(content, tickloop_agent::model::Content::Text { text } if text == "Weekly selected."))) { break; }
        tokio::task::yield_now().await;
    }}).await.unwrap();
    assert_eq!(model.requests.lock().unwrap().len(), 2);
    runtime.shutdown().await;
}

#[tokio::test]
async fn after_turn_failures_are_observable_and_do_not_block_the_next_queued_action() {
    use tickloop_agent::agent_access::AfterTurnAction;
    let temporary = tempfile::tempdir().unwrap();
    let runtime = Agent::new_embedded(
        support::config(),
        Arc::new(support::ScriptedModel::new(vec![support::text(
            "Next action ran.",
        )])),
    );
    let access = EmbeddedAgentAccess::new(runtime.clone());
    let created = access
        .execute(CommandEnvelope {
            owner_epoch: access.owner_epoch(),
            command_id: Uuid::new_v4(),
            command: AgentCommand::Create {
                launch: LaunchSpec::embedded(temporary.path().to_path_buf()),
                initial: None,
                origin: None,
            },
        })
        .await
        .unwrap()
        .into_result()
        .unwrap();
    let CommandResult::Created { execution, .. } = created else {
        panic!("expected Create");
    };
    let queued = access
        .execute(CommandEnvelope {
            owner_epoch: access.owner_epoch(),
            command_id: Uuid::new_v4(),
            command: AgentCommand::EnqueueAfterTurn {
                execution,
                target_loop: None,
                action: AfterTurnAction::Submit {
                    input: UserInput::Skill {
                        name: "fixture-missing-skill".into(),
                        arguments: String::new(),
                    },
                    origin: None,
                },
            },
        })
        .await
        .unwrap()
        .into_result()
        .unwrap();
    let CommandResult::Queued { command_id } = queued else {
        panic!("expected queued command");
    };
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let snapshot = live(&access, execution).await;
            if snapshot
                .queue_failures
                .iter()
                .any(|failure| failure.command_id == command_id)
            {
                assert!(
                    snapshot
                        .warnings
                        .iter()
                        .any(|warning| warning.contains("fixture-missing-skill"))
                );
                assert!(snapshot.pending_commands.is_empty());
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    access
        .execute(CommandEnvelope {
            owner_epoch: access.owner_epoch(),
            command_id: Uuid::new_v4(),
            command: AgentCommand::EnqueueAfterTurn {
                execution,
                target_loop: None,
                action: AfterTurnAction::Submit {
                    input: UserInput::Text {
                        text: "Continue with the next action".into(),
                        contexts: vec![],
                        paths: vec![],
                    },
                    origin: None,
                },
            },
        })
        .await
        .unwrap()
        .into_result()
        .unwrap();
    tokio::time::timeout(Duration::from_secs(3), async { loop {
        if live(&access, execution).await.conversation.messages.iter().any(|message| message.content.iter().any(|content| matches!(content, tickloop_agent::model::Content::Text { text } if text == "Next action ran."))) { break; }
        tokio::task::yield_now().await;
    }}).await.unwrap();
    runtime.shutdown().await;
}

#[tokio::test]
async fn a_permission_storage_failure_preserves_the_request_for_an_explicit_retry() {
    use tickloop_agent::{agent_access::AccessErrorCode, permissions::PermissionScope};
    let temporary = tempfile::tempdir().unwrap();
    let external = tempfile::tempdir().unwrap();
    let source = external.path().join("source.txt");
    std::fs::write(&source, "approved information").unwrap();
    let root = temporary.path().join("state");
    let model = Arc::new(support::ScriptedModel::new(vec![
        support::call("read", "Read", serde_json::json!({"file_path": source})),
        support::text("The explicit retry completed."),
    ]));
    let runtime = Agent::open_embedded(root.clone(), support::config(), model.clone()).unwrap();
    let access = EmbeddedAgentAccess::new(runtime.clone());
    let created = access
        .execute(CommandEnvelope {
            owner_epoch: access.owner_epoch(),
            command_id: Uuid::new_v4(),
            command: AgentCommand::Create {
                launch: LaunchSpec::embedded(temporary.path().to_path_buf()),
                initial: Some(UserInput::Text {
                    text: "Read the external source".into(),
                    contexts: vec![],
                    paths: vec![],
                }),
                origin: None,
            },
        })
        .await
        .unwrap()
        .into_result()
        .unwrap();
    let CommandResult::Created { execution, .. } = created else {
        panic!("expected Create");
    };
    let request = tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if let Some(request) = live(&access, execution)
                .await
                .request_refs
                .into_iter()
                .find(|request| request.kind == RequestKind::Permission)
            {
                break request;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    std::fs::create_dir(root.join("settings.json")).unwrap();
    let failed = access
        .execute(CommandEnvelope {
            owner_epoch: access.owner_epoch(),
            command_id: Uuid::new_v4(),
            command: AgentCommand::DecidePermission {
                request,
                allow: true,
                scope: PermissionScope::Persistent,
                comment: None,
                rule: None,
            },
        })
        .await
        .unwrap();
    assert_eq!(failed.error.unwrap().code, AccessErrorCode::Storage);
    let retry = live(&access, execution).await.request_refs[0];
    assert_eq!(retry.request_id, request.request_id);
    assert_eq!(retry.version, request.version + 1);
    std::fs::remove_dir(root.join("settings.json")).unwrap();
    access
        .execute(CommandEnvelope {
            owner_epoch: access.owner_epoch(),
            command_id: Uuid::new_v4(),
            command: AgentCommand::DecidePermission {
                request: retry,
                allow: true,
                scope: PermissionScope::Once,
                comment: None,
                rule: None,
            },
        })
        .await
        .unwrap()
        .into_result()
        .unwrap();
    tokio::time::timeout(Duration::from_secs(3), async { loop {
        if live(&access, execution).await.conversation.messages.iter().any(|message| message.content.iter().any(|content| matches!(content, tickloop_agent::model::Content::Text { text } if text == "The explicit retry completed."))) { break; }
        tokio::task::yield_now().await;
    }}).await.unwrap();
    assert_eq!(model.requests.lock().unwrap().len(), 2);
    runtime.shutdown().await;
}

#[tokio::test]
async fn oversized_execution_updates_request_resync_without_losing_the_output() {
    use tickloop_agent::agent_access::{FEED_CAPACITY, FEED_MAX_BYTES, ObservationChange};
    let temporary = tempfile::tempdir().unwrap();
    let output = "result ".repeat(20_000);
    let runtime = Agent::new_embedded(
        support::config(),
        Arc::new(support::ScriptedModel::new(vec![support::text(&output)])),
    );
    let access = EmbeddedAgentAccess::new(runtime.clone());
    let created = access
        .execute(CommandEnvelope {
            owner_epoch: access.owner_epoch(),
            command_id: Uuid::new_v4(),
            command: AgentCommand::Create {
                launch: LaunchSpec::embedded(temporary.path().to_path_buf()),
                initial: None,
                origin: None,
            },
        })
        .await
        .unwrap()
        .into_result()
        .unwrap();
    let CommandResult::Created { execution, .. } = created else {
        panic!("expected Create");
    };
    let mut observation = access
        .observe(ObservationRequest::Agent {
            execution,
            detail: AgentDetailQuery::Snapshot,
            after: None,
        })
        .await
        .unwrap();
    access
        .execute(CommandEnvelope {
            owner_epoch: access.owner_epoch(),
            command_id: Uuid::new_v4(),
            command: AgentCommand::Submit {
                execution,
                target_loop: None,
                input: UserInput::Text {
                    text: "Return the complete result".into(),
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
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let update = observation.recv().await.unwrap();
            assert!(serde_json::to_vec(&update).unwrap().len() <= FEED_MAX_BYTES / FEED_CAPACITY);
            if matches!(update.change, ObservationChange::Resync { .. }) {
                break;
            }
        }
    })
    .await
    .unwrap();
    tokio::time::timeout(Duration::from_secs(3), async { loop {
        if live(&access, execution).await.conversation.messages.iter().any(|message| message.content.iter().any(|content| matches!(content, tickloop_agent::model::Content::Text { text } if text == &output))) { break; }
        tokio::task::yield_now().await;
    }}).await.unwrap();
    runtime.shutdown().await;
}

#[tokio::test]
async fn embedded_directory_observation_reports_a_new_execution_after_its_snapshot() {
    use tickloop_agent::agent_access::{AgentFilter, Page};
    let temporary = tempfile::tempdir().unwrap();
    let runtime = Agent::new_embedded(
        support::config(),
        Arc::new(support::ScriptedModel::new(vec![])),
    );
    let access = EmbeddedAgentAccess::new(runtime.clone());
    let query = ObservationRequest::Overview {
        filter: AgentFilter::default(),
        page: Page {
            offset: 0,
            limit: 256,
        },
        after: None,
    };
    let mut observation = access.observe(query.clone()).await.unwrap();
    assert!(
        matches!(&observation.initial, ObservationValue::Overview(entries) if entries.is_empty())
    );
    let created = access
        .execute(CommandEnvelope {
            owner_epoch: access.owner_epoch(),
            command_id: Uuid::new_v4(),
            command: AgentCommand::Create {
                launch: LaunchSpec::embedded(temporary.path().to_path_buf()),
                initial: None,
                origin: None,
            },
        })
        .await
        .unwrap()
        .into_result()
        .unwrap();
    let CommandResult::Created { execution, .. } = created else {
        panic!("expected Create");
    };
    let update = tokio::time::timeout(Duration::from_secs(2), observation.recv())
        .await
        .unwrap()
        .unwrap();
    assert!(update.cursor.sequence > observation.cursor.sequence);
    assert!(
        matches!(access.observe(query).await.unwrap().initial, ObservationValue::Overview(entries) if entries.len() == 1 && entries[0].execution == execution)
    );
    runtime.shutdown().await;
}

#[tokio::test]
async fn overview_titles_name_each_agent_by_its_first_task_including_user_shells() {
    use tickloop_agent::agent_access::{AgentFilter, Page};
    let temporary = tempfile::tempdir().unwrap();
    let runtime = Agent::new_embedded(
        support::config(),
        Arc::new(support::ScriptedModel::new(vec![support::text("noted")])),
    );
    let access = EmbeddedAgentAccess::new(runtime.clone());
    let mut expected = Vec::new();
    for (initial, title) in [
        (
            Some(UserInput::Shell {
                command: "printf alpha".into(),
                background: true,
            }),
            "!printf alpha",
        ),
        (
            Some(UserInput::Text {
                text: "Summarize the alpha notes".into(),
                contexts: vec![],
                paths: vec![],
            }),
            "Summarize the alpha notes",
        ),
        (None, "New Agent"),
    ] {
        let created = access
            .execute(CommandEnvelope {
                owner_epoch: access.owner_epoch(),
                command_id: Uuid::new_v4(),
                command: AgentCommand::Create {
                    launch: LaunchSpec::embedded(temporary.path().to_path_buf()),
                    initial,
                    origin: None,
                },
            })
            .await
            .unwrap()
            .into_result()
            .unwrap();
        let CommandResult::Created { execution, .. } = created else {
            panic!("expected Create");
        };
        expected.push((execution, title));
    }
    let ObservationValue::Overview(entries) = access
        .observe(ObservationRequest::Overview {
            filter: AgentFilter::default(),
            page: Page {
                offset: 0,
                limit: 256,
            },
            after: None,
        })
        .await
        .unwrap()
        .initial
    else {
        panic!("expected overview");
    };
    for (execution, title) in expected {
        let entry = entries
            .iter()
            .find(|entry| entry.execution == execution)
            .unwrap();
        assert_eq!(entry.title, title);
    }
    runtime.shutdown().await;
}

#[tokio::test]
async fn creating_with_an_unknown_skill_rejects_without_registering_an_empty_agent() {
    let directory = tempfile::tempdir().unwrap();
    let runtime = Agent::new_embedded(
        support::config(),
        Arc::new(support::ScriptedModel::new(vec![support::text("loaded")])),
    );
    let access = EmbeddedAgentAccess::new(runtime.clone());
    let create = |name: &str| CommandEnvelope {
        owner_epoch: access.owner_epoch(),
        command_id: Uuid::new_v4(),
        command: AgentCommand::Create {
            launch: LaunchSpec::embedded(PathBuf::from(directory.path())),
            initial: Some(UserInput::Skill {
                name: name.into(),
                arguments: String::new(),
            }),
            origin: None,
        },
    };
    let rejected = access.execute(create("model")).await;
    let error = match rejected {
        Ok(receipt) => receipt.into_result().unwrap_err(),
        Err(error) => error,
    };
    assert!(error.message.contains("model"), "{error:?}");
    let catalog = |access: &EmbeddedAgentAccess| {
        let access = access.clone();
        async move {
            let observation = access
                .observe(ObservationRequest::Overview {
                    filter: Default::default(),
                    page: tickloop_agent::agent_access::Page {
                        offset: 0,
                        limit: 16,
                    },
                    after: None,
                })
                .await
                .unwrap();
            let ObservationValue::Overview(rows) = observation.initial else {
                panic!("expected the overview catalog");
            };
            rows
        }
    };
    assert!(
        catalog(&access).await.is_empty(),
        "a rejected first Skill leaves no empty Agent behind"
    );
    let accepted = access.execute(create("tickloop")).await.unwrap();
    assert!(accepted.into_result().is_ok());
    assert_eq!(catalog(&access).await.len(), 1);
    runtime.shutdown().await;
}
