use super::{
    console::{Console, InputEvent},
    execution::ConsoleExecution,
};
use crate::{
    artifacts::{ArtifactClient, ArtifactError},
    runtime::TickSubscriptionChange,
};
use std::path::Path;
use uuid::Uuid;

pub(super) async fn refresh(
    agent: &ConsoleExecution,
    console: &Console,
    root: Option<&Path>,
    chat: Uuid,
) -> Result<(), String> {
    let snapshot = agent
        .snapshot(chat)
        .await
        .map_err(|error| error.to_string())?;
    console.update_artifact_ticks(&snapshot)?;
    let Some(root) = root else {
        return Ok(());
    };
    use futures_util::StreamExt;
    const ARTIFACT_REFRESH_CONCURRENCY: usize = 4;
    let sink = console.artifact_metadata_sink();
    let root = root.to_path_buf();
    tokio::spawn(async move {
        futures_util::stream::iter(snapshot.artifact_ticks)
            .for_each_concurrent(ARTIFACT_REFRESH_CONCURRENCY, |tick| {
                let sink = &sink;
                let root = &root;
                async move {
                    let Ok(client) = ArtifactClient::for_origin(root, &tick.origin) else {
                        return;
                    };
                    match client.read(&tick.artifact_id).await {
                        Ok(metadata) => sink.update(chat, tick.instance_id(), Some(metadata)),
                        Err(ArtifactError::Service { status: 404, .. }) => {
                            sink.update(chat, tick.instance_id(), None)
                        }
                        Err(_) => {}
                    }
                }
            })
            .await;
    });
    Ok(())
}

pub(super) async fn tick_action(
    agent: &ConsoleExecution,
    console: &Console,
    root: Option<&Path>,
    chat: Uuid,
    id: String,
    action: char,
) -> Result<Option<InputEvent>, String> {
    if action == 'r' {
        refresh(agent, console, root, chat).await?;
        return Ok(None);
    }
    if let Ok(subscription_id) = Uuid::parse_str(&id) {
        let rows = agent
            .tick_subscriptions(chat)
            .await
            .map_err(|error| error.to_string())?;
        console.show_subscriptions_at(chat, rows, subscription_id, action == 'x')?;
        return Ok(None);
    }
    let ticks = agent
        .artifact_ticks(chat)
        .await
        .map_err(|error| error.to_string())?;
    let Some(tick) = ticks.into_iter().find(|tick| tick.instance_id() == id) else {
        return Ok(None);
    };
    match action {
        'o' => {
            if let Err(error) = super::login::open_artifact(&tick.url).await {
                console.set_status(&format!("{error} · press c to copy the link"))?;
            }
        }
        'c' => console.copy_artifact_link(&tick.url)?,
        'x' | 'X' => {
            let rows = agent
                .tick_subscriptions(chat)
                .await
                .map_err(|error| error.to_string())?;
            let subscriptions: Vec<_> = rows
                .into_iter()
                .filter(|row| row.instance.as_str() == id)
                .collect();
            if action == 'x' && !subscriptions.is_empty() {
                console.confirm_tick_removal(chat, id)?;
                return Ok(None);
            }
            for row in subscriptions {
                agent
                    .update_tick_subscription(chat, row.id, TickSubscriptionChange::Cancel)
                    .await
                    .map_err(|error| error.to_string())?;
            }
            agent
                .remove_artifact_tick(chat, id)
                .await
                .map_err(|error| error.to_string())?;
            let snapshot = agent
                .snapshot(chat)
                .await
                .map_err(|error| error.to_string())?;
            console.update_artifact_ticks(&snapshot)?;
        }
        'f' => {
            if tick.access_role != "owner" {
                console.set_status("Only the owner can receive artifact feedback")?;
                return Ok(None);
            }
            let snapshot = agent
                .snapshot(chat)
                .await
                .map_err(|error| error.to_string())?;
            let paused: Vec<_> = snapshot
                .loops
                .iter()
                .filter(|goal| {
                    goal.event_sources.iter().any(|source| {
                        source.source_key == tick.artifact_id && source.resume_required
                    })
                })
                .map(|goal| goal.id)
                .collect();
            if !paused.is_empty() {
                for loop_id in paused {
                    agent
                        .resume_event_sources(chat, loop_id)
                        .await
                        .map_err(|error| error.to_string())?;
                }
                console.set_status("Artifact feedback resumed here")?;
            } else if snapshot.loops.iter().any(|goal| {
                goal.event_sources
                    .iter()
                    .any(|source| source.source_key == tick.artifact_id)
            }) {
                console.set_status("Artifact feedback is already connected here")?;
            } else {
                let takeover = if let Some(root) = root {
                    if let Ok(client) = ArtifactClient::for_origin(root, &tick.origin) {
                        client
                            .read(&tick.artifact_id)
                            .await
                            .ok()
                            .and_then(|metadata| metadata.association)
                            .is_some()
                    } else {
                        false
                    }
                } else {
                    false
                };
                let text = format!(
                    "Connect artifact {} ({}) to this Chat to receive its feedback here.{}",
                    tick.artifact_id,
                    tick.url,
                    if takeover {
                        " I explicitly request taking over its existing feedback association."
                    } else {
                        ""
                    }
                );
                return Ok(Some(InputEvent::Prompt {
                    text,
                    file_references: Vec::new(),
                    images: Vec::new(),
                }));
            }
        }
        _ => {}
    }
    Ok(None)
}

pub(super) async fn open(
    agent: &ConsoleExecution,
    console: &Console,
    root: &Path,
    chat: Option<Uuid>,
    interactive: bool,
) -> Result<(), String> {
    let client = ArtifactClient::new(root).map_err(|error| error.to_string())?;
    let account = client
        .status()
        .await
        .map_err(|error| error.to_string())?
        .ok_or("Artifact Service is signed out")?;
    if !interactive {
        let page = client
            .list("all", "", None)
            .await
            .map_err(|error| error.to_string())?;
        for artifact in page.artifacts.iter().take(20) {
            console.write_line(&super::safe_text(&format!(
                "{} · {} · {} · {}",
                artifact.title, artifact.format, artifact.access_role, artifact.url
            )))?;
        }
        return Ok(());
    }
    let added = if let Some(chat) = chat {
        refresh(agent, console, Some(root), chat).await?;
        agent
            .artifact_ticks(chat)
            .await
            .map_err(|error| error.to_string())?
            .into_iter()
            .filter(|tick| tick.origin == client.origin())
            .map(|tick| tick.artifact_id)
            .collect()
    } else {
        Default::default()
    };
    console.show_artifacts_picker(super::artifacts_picker::ArtifactsPicker::new(
        chat,
        account.email,
        added,
    ))
}

pub(super) async fn load_page(
    root: &Path,
    request: &super::artifacts_picker::ArtifactLoad,
) -> Result<
    (
        crate::artifacts::ArtifactPage,
        std::collections::HashSet<String>,
        bool,
    ),
    String,
> {
    let client = ArtifactClient::new(root).map_err(|error| error.to_string())?;
    let pinned_page = client.pinned().await.map_err(|error| error.to_string())?;
    let pinned = pinned_page
        .artifacts
        .iter()
        .map(|artifact| artifact.id.clone())
        .collect();
    if request.pinned {
        return Ok((pinned_page, pinned, false));
    }
    match client
        .list(request.ownership, &request.query, request.cursor.as_deref())
        .await
    {
        Ok(page) => Ok((page, pinned, false)),
        Err(ArtifactError::Service { code, .. }) if code == "catalog_conflict" => client
            .list(request.ownership, &request.query, None)
            .await
            .map(|page| (page, pinned, true))
            .map_err(|error| error.to_string()),
        Err(error) => Err(error.to_string()),
    }
}

pub(super) async fn apply_choice(
    agent: &ConsoleExecution,
    console: &Console,
    root: &Path,
    chat: Option<Uuid>,
    choice: super::artifacts_picker::ArtifactChoice,
) -> Result<(), String> {
    use super::artifacts_picker::ArtifactPickerAction;
    let client = ArtifactClient::new(root).map_err(|error| error.to_string())?;
    let artifact = choice.artifact;
    match choice.action {
        ArtifactPickerAction::Add => {
            let chat = chat.ok_or("Select a Chat before adding an artifact")?;
            let tick = artifact.tick(client.origin(), crate::private_fs::unix_time_milliseconds());
            let id = tick.instance_id();
            agent
                .add_artifact_tick(chat, tick)
                .await
                .map_err(|error| error.to_string())?;
            console.update_artifact_ticks(
                &agent
                    .snapshot(chat)
                    .await
                    .map_err(|error| error.to_string())?,
            )?;
            console.update_artifact_metadata(chat, id.clone(), Some(artifact))?;
            console.select_artifact_tick(chat, &id)?;
        }
        ArtifactPickerAction::Open => super::login::open_artifact(&artifact.url)
            .await
            .map_err(|error| format!("{error} · c copies the link"))?,
        ArtifactPickerAction::Copy => console.copy_artifact_link(&artifact.url)?,
        ArtifactPickerAction::Pin(pinned) => {
            client
                .pin(&artifact.id, pinned)
                .await
                .map_err(|error| error.to_string())?;
            console.refresh_artifacts_picker()?;
        }
        ArtifactPickerAction::Rename(title) => {
            if !matches!(artifact.access_role.as_str(), "owner" | "editor") {
                return Err("Only owners and editors can rename artifacts".into());
            }
            let operation = client
                .rename(&artifact.id, &title, artifact.metadata_revision)
                .await
                .map_err(|error| error.to_string())?;
            if operation.status != "committed" {
                return Err(format!(
                    "Rename operation {}: {} {:?}",
                    operation.operation_id, operation.status, operation.error
                ));
            }
            console.refresh_artifacts_picker()?;
            if let Some(chat) = chat {
                refresh(agent, console, Some(root), chat).await?;
            }
        }
        ArtifactPickerAction::Delete => {
            if artifact.access_role != "owner" {
                return Err("Only the owner can delete artifacts".into());
            }
            let operation = client
                .delete(&artifact.id)
                .await
                .map_err(|error| error.to_string())?;
            if operation.status != "committed" {
                return Err(format!(
                    "Delete operation {}: {} {:?}",
                    operation.operation_id, operation.status, operation.error
                ));
            }
            if let Some(chat) = chat {
                let instance = format!("artifact:{}/{}", client.origin(), artifact.id);
                for subscription in agent
                    .tick_subscriptions(chat)
                    .await
                    .map_err(|error| error.to_string())?
                    .into_iter()
                    .filter(|row| row.instance.as_str() == instance)
                {
                    agent
                        .update_tick_subscription(
                            chat,
                            subscription.id,
                            TickSubscriptionChange::Cancel,
                        )
                        .await
                        .map_err(|error| {
                            format!(
                                "Artifact deleted; cannot cancel its local subscription: {error}"
                            )
                        })?;
                }
                agent
                    .remove_artifact_tick(chat, instance)
                    .await
                    .map_err(|error| error.to_string())?;
                console.update_artifact_ticks(
                    &agent
                        .snapshot(chat)
                        .await
                        .map_err(|error| error.to_string())?,
                )?;
            }
            console.refresh_artifacts_picker()?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::terminal::test_support;
    use crate::{
        agent_access::*,
        runtime::{EventSourceBinding, LoopSnapshot, LoopState, Receipt, SourcePublication},
    };
    use std::sync::{Arc, Mutex};

    struct FeedbackRemote {
        inner: EmbeddedAgentAccess,
        loop_id: Uuid,
        resumed: Mutex<Vec<Uuid>>,
    }
    #[async_trait::async_trait]
    impl AgentAccess for FeedbackRemote {
        async fn observe(&self, request: ObservationRequest) -> Result<Observation, AccessError> {
            let mut observation = self.inner.observe(request).await?;
            if let ObservationValue::Agent(snapshot) = &mut observation.initial {
                let chat = snapshot.conversation.id;
                snapshot.conversation.loops.push(LoopSnapshot {
                    id: self.loop_id,
                    goal: "artifact feedback".into(),
                    state: LoopState::Interrupted,
                    run_id: None,
                    trigger: None,
                    state_since: None,
                    event_sources: vec![EventSourceBinding {
                        server_id: "artifacts".into(),
                        owner_account_id: "owner".into(),
                        source_key: "doc".into(),
                        publication: Some(SourcePublication {
                            version_id: "version".into(),
                            operation_id: "publication".into(),
                            receipt: Receipt {
                                chat_id: chat,
                                loop_id: self.loop_id,
                                run_id: Uuid::new_v4(),
                            },
                        }),
                        publication_history: Vec::new(),
                        connection: None,
                        association_operation_id: "association".into(),
                        binding_id: Uuid::new_v4(),
                        client_instance_id: Uuid::new_v4(),
                        credential_generation: [0; 32],
                        directory_fingerprint: [0; 32],
                        bridge_fingerprint: [0; 32],
                        association_epoch: Some(1),
                        resume_required: true,
                        association_unknown: false,
                        association_rejected: None,
                        pending_cursor: None,
                        claim_operations: Vec::new(),
                    }],
                });
            }
            Ok(observation)
        }
        async fn execute(&self, envelope: CommandEnvelope) -> Result<CommandReceipt, AccessError> {
            if let AgentCommand::ManageLoop {
                action: LoopAction::ResumeEventSources { loop_id },
                ..
            } = &envelope.command
            {
                self.resumed.lock().unwrap().push(*loop_id);
                let mut receipt = self
                    .inner
                    .execute(CommandEnvelope {
                        command: AgentCommand::FlushTranscripts,
                        ..envelope
                    })
                    .await?;
                receipt.result = Some(CommandResult::EventSourcesResumed { count: 1 });
                return Ok(receipt);
            }
            self.inner.execute(envelope).await
        }
        async fn show_view(&self, update: DisplayUpdate) -> Result<DisplayReceipt, AccessError> {
            self.inner.show_view(update).await
        }
    }

    #[tokio::test]
    async fn feedback_key_resumes_the_exact_paused_loop_without_submitting_a_new_prompt() {
        let directory = tempfile::tempdir().unwrap();
        let model = Arc::new(test_support::ScriptedModel::new(Vec::new()));
        let runtime = crate::runtime::Agent::new_embedded(test_support::config(), model.clone());
        let inner = EmbeddedAgentAccess::new(runtime.clone());
        let remote = Arc::new(FeedbackRemote {
            inner,
            loop_id: Uuid::new_v4(),
            resumed: Mutex::new(Vec::new()),
        });
        let agent = ConsoleExecution::from_access_for_test(
            remote.clone(),
            remote.inner.owner_epoch(),
            ViewRef {
                session_id: Uuid::new_v4(),
                view_id: Uuid::new_v4(),
            },
            LaunchSpec::embedded(directory.path().into()),
            None,
        );
        let chat = agent.create_chat(directory.path().into()).await.unwrap();
        let tick = crate::artifacts::ArtifactTick {
            artifact_id: "doc".into(),
            origin: "https://docs.example".into(),
            title: "Feedback".into(),
            format: "markdown".into(),
            url: "https://docs.example/a/doc".into(),
            access_role: "owner".into(),
            added_at_ms: 1,
        };
        agent.add_artifact_tick(chat, tick.clone()).await.unwrap();
        let console = Console::test_for_chat(chat);
        let outcome = tick_action(&agent, &console, None, chat, tick.instance_id(), 'f')
            .await
            .unwrap();
        assert!(outcome.is_none());
        assert_eq!(*remote.resumed.lock().unwrap(), vec![remote.loop_id]);
        assert!(model.requests.lock().unwrap().is_empty());
        runtime.shutdown().await;
    }
}
