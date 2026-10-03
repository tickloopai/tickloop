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
                    let client = match ArtifactClient::for_origin(root, &tick.origin) {
                        Ok(client) => client,
                        Err(error) => {
                            sink.failed(chat, tick.instance_id(), error.to_string());
                            return;
                        }
                    };
                    match client.read(&tick.artifact_id).await {
                        Ok(metadata) => sink.update(chat, tick.instance_id(), Some(metadata)),
                        Err(error @ ArtifactError::Service { status: 404, .. }) => {
                            sink.update(chat, tick.instance_id(), None);
                            sink.failed(chat, tick.instance_id(), error.to_string());
                        }
                        Err(error) => sink.failed(chat, tick.instance_id(), error.to_string()),
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
    let mut subscribed_tick = None;
    if let Ok(subscription_id) = Uuid::parse_str(&id) {
        let rows = agent
            .tick_subscriptions(chat)
            .await
            .map_err(|error| error.to_string())?;
        let artifact = rows
            .iter()
            .find(|row| row.id == subscription_id)
            .and_then(|row| crate::tick_adapter::artifact_target(&row.instance));
        let Some((origin, artifact_id)) = artifact.filter(|_| matches!(action, 'o' | 'c' | 'f'))
        else {
            console.show_subscriptions_at(chat, rows, subscription_id, action == 'x')?;
            return Ok(None);
        };
        let root = root.ok_or("Artifact Service configuration is unavailable")?;
        let client =
            ArtifactClient::for_origin(root, &origin).map_err(|error| error.to_string())?;
        let metadata = client
            .read(&artifact_id)
            .await
            .map_err(|error| error.to_string())?;
        subscribed_tick = Some(metadata.tick(&origin, 0));
    }
    let ticks = agent
        .artifact_ticks(chat)
        .await
        .map_err(|error| error.to_string())?;
    let tick = subscribed_tick.or_else(|| ticks.into_iter().find(|tick| tick.instance_id() == id));
    let tick = if let Some(tick) = tick {
        tick
    } else {
        let rows = agent
            .tick_subscriptions(chat)
            .await
            .map_err(|error| error.to_string())?;
        let target = rows
            .iter()
            .find(|row| row.instance.as_str() == id)
            .and_then(|row| crate::tick_adapter::artifact_target(&row.instance));
        let Some((origin, artifact_id)) = target else {
            return Ok(None);
        };
        if matches!(action, 'x' | 'X') {
            if action == 'x' {
                console.confirm_tick_removal(chat, id)?;
            } else {
                remove_local_artifact(agent, console, chat, &id).await?;
            }
            return Ok(None);
        }
        let client = ArtifactClient::for_origin(
            root.ok_or("Artifact Service configuration is unavailable")?,
            &origin,
        )
        .map_err(|error| error.to_string())?;
        client
            .read(&artifact_id)
            .await
            .map_err(|error| error.to_string())?
            .tick(&origin, 0)
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
            remove_local_artifact(agent, console, chat, &id).await?;
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
                let root = root.ok_or("Artifact Service configuration is unavailable")?;
                let client = ArtifactClient::for_origin(root, &tick.origin)
                    .map_err(|error| error.to_string())?;
                let metadata = client.read(&tick.artifact_id).await.map_err(|error| {
                    format!("Cannot check artifact feedback association: {error}")
                })?;
                if metadata.access_role != "owner" {
                    return Err("Only the owner can receive artifact feedback".into());
                }
                let takeover = metadata.association.is_some();
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

async fn remove_local_artifact(
    agent: &ConsoleExecution,
    console: &Console,
    chat: Uuid,
    instance: &str,
) -> Result<(), String> {
    for subscription in agent
        .tick_subscriptions(chat)
        .await
        .map_err(|error| error.to_string())?
        .into_iter()
        .filter(|row| row.instance.as_str() == instance)
    {
        agent
            .update_tick_subscription(chat, subscription.id, TickSubscriptionChange::Cancel)
            .await
            .map_err(|error| error.to_string())?;
    }
    agent
        .remove_artifact_tick(chat, instance.to_owned())
        .await
        .map_err(|error| error.to_string())?;
    console.update_subscribed_ticks(
        chat,
        &agent
            .tick_subscriptions(chat)
            .await
            .map_err(|error| error.to_string())?,
    )?;
    console.update_artifact_ticks(
        &agent
            .snapshot(chat)
            .await
            .map_err(|error| error.to_string())?,
    )
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
    let client = ArtifactClient::new(root).map_err(|error| error.to_string())?;
    apply_choice_with_client(agent, console, root, chat, choice, &client).await
}

async fn apply_choice_with_client(
    agent: &ConsoleExecution,
    console: &Console,
    root: &Path,
    chat: Option<Uuid>,
    choice: super::artifacts_picker::ArtifactChoice,
    client: &ArtifactClient,
) -> Result<(), String> {
    use super::artifacts_picker::ArtifactPickerAction;
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
            console.select_artifact_tick(choice.picker, chat, &id)?;
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
                let instance =
                    tick_connect::instance::artifact_instance_id(client.origin(), &artifact.id);
                remove_local_artifact(agent, console, chat, &instance)
                    .await
                    .map_err(|error| format!("Artifact deleted; local cleanup failed: {error}"))?;
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

    struct SubscriptionRemote {
        inner: EmbeddedAgentAccess,
        rows: Mutex<Vec<crate::runtime::TickSubscriptionStatus>>,
        cancelled: Mutex<Vec<Uuid>>,
    }
    #[async_trait::async_trait]
    impl AgentAccess for SubscriptionRemote {
        async fn observe(&self, request: ObservationRequest) -> Result<Observation, AccessError> {
            let mut observation = self.inner.observe(request).await?;
            if let ObservationValue::Subscriptions(rows) = &mut observation.initial {
                *rows = self.rows.lock().unwrap().clone();
            }
            Ok(observation)
        }
        async fn execute(&self, envelope: CommandEnvelope) -> Result<CommandReceipt, AccessError> {
            if let AgentCommand::ManageLoop {
                action:
                    LoopAction::UpdateSubscription {
                        subscription_id,
                        change: TickSubscriptionChange::Cancel,
                    },
                ..
            } = &envelope.command
            {
                self.cancelled.lock().unwrap().push(*subscription_id);
                self.rows
                    .lock()
                    .unwrap()
                    .retain(|row| row.id != *subscription_id);
                let mut receipt = self
                    .inner
                    .execute(CommandEnvelope {
                        command: AgentCommand::FlushTranscripts,
                        ..envelope
                    })
                    .await?;
                receipt.result = Some(CommandResult::Done);
                return Ok(receipt);
            }
            self.inner.execute(envelope).await
        }
        async fn show_view(&self, update: DisplayUpdate) -> Result<DisplayReceipt, AccessError> {
            self.inner.show_view(update).await
        }
    }

    async fn artifact_service() -> (String, tokio::task::JoinHandle<()>, Arc<Mutex<Vec<String>>>) {
        use tokio::{
            io::{AsyncReadExt, AsyncWriteExt},
            net::TcpListener,
        };
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        let endpoint = origin.clone();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let captured = requests.clone();
        let task = tokio::spawn(async move {
            loop {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut bytes = vec![0_u8; 8192];
                let count = socket.read(&mut bytes).await.unwrap();
                let request = String::from_utf8_lossy(&bytes[..count]);
                captured
                    .lock()
                    .unwrap()
                    .push(request.lines().next().unwrap().to_owned());
                let user =
                    serde_json::json!({"id":"owner","name":"Owner","email":"owner@example.test"});
                let body = if request.starts_with("POST /api/v1/auth/device ") {
                    serde_json::json!({"device_code":"device","user_code":"CODE","verification_uri":format!("{endpoint}/device?code=CODE"),"expires_in":60,"interval":1})
                } else if request.starts_with("POST /api/v1/auth/device/token ") {
                    serde_json::json!({"access_token":"test-token","token_type":"Bearer","expires_in":600,"user":user})
                } else if request.starts_with("DELETE /api/v1/artifacts/doc ") {
                    let input: serde_json::Value =
                        serde_json::from_str(request.split("\r\n\r\n").nth(1).unwrap()).unwrap();
                    serde_json::json!({"operation_id":input["operation_id"],"status":"committed"})
                } else {
                    serde_json::json!({"id":"doc","title":"Subscribed document","format":"markdown","access_role":"owner","url":format!("{endpoint}/a/doc"),"association":{"chat_id":Uuid::new_v4()}})
                };
                let body = body.to_string();
                socket.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes()).await.unwrap();
            }
        });
        (origin, task, requests)
    }

    async fn subscribed_fixture(
        directory: &Path,
        origin: &str,
    ) -> (
        ConsoleExecution,
        crate::runtime::Agent,
        Arc<SubscriptionRemote>,
        Uuid,
    ) {
        let core = crate::runtime::Agent::new_embedded(
            test_support::config(),
            Arc::new(test_support::ScriptedModel::new(Vec::new())),
        );
        let inner = EmbeddedAgentAccess::new(core.clone());
        let remote = Arc::new(SubscriptionRemote {
            inner,
            rows: Mutex::new(Vec::new()),
            cancelled: Mutex::new(Vec::new()),
        });
        let agent = ConsoleExecution::from_access_for_test(
            remote.clone(),
            remote.inner.owner_epoch(),
            ViewRef {
                session_id: Uuid::new_v4(),
                view_id: Uuid::new_v4(),
            },
            LaunchSpec::embedded(directory.into()),
            None,
        );
        let chat = agent.create_chat(directory.into()).await.unwrap();
        remote.rows.lock().unwrap().push(serde_json::from_value(serde_json::json!({"id":Uuid::new_v4(),"loop_id":Uuid::new_v4(),"loop_state":"WaitingTrigger","instance":tick_connect::instance::artifact_instance_id(origin,"doc"),"every_seconds":60,"accumulated":0,"last_delivery_ms":null,"last_delivery":null,"link":"Live","kind":"Protocol","title":"Subscribed document","updated_at_ms":1000})).unwrap());
        (agent, core, remote, chat)
    }

    #[tokio::test]
    async fn subscribed_artifact_open_copy_and_feedback_use_its_actual_type_without_adding_it() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let (origin, server, _) = artifact_service().await;
        let client = ArtifactClient::for_origin(&root, &origin).unwrap();
        let device = client.start_device_login().await.unwrap();
        client
            .finish_device_login(device, &tokio_util::sync::CancellationToken::new())
            .await
            .unwrap();
        let (agent, core, remote, chat) = subscribed_fixture(directory.path(), &origin).await;
        let id = remote.rows.lock().unwrap()[0].id.to_string();
        let console = Console::test_for_chat(chat);
        tick_action(&agent, &console, Some(&root), chat, id.clone(), 'o')
            .await
            .unwrap();
        assert!(
            console
                .test_status_text()
                .contains("Only HTTPS artifact links can be opened")
        );
        tick_action(&agent, &console, Some(&root), chat, id.clone(), 'c')
            .await
            .unwrap();
        assert_eq!(
            console.test_clipboard_contents(),
            vec![format!("{origin}/a/doc")]
        );
        let Some(InputEvent::Prompt { text, .. }) =
            tick_action(&agent, &console, Some(&root), chat, id, 'f')
                .await
                .unwrap()
        else {
            panic!("artifact feedback action did not produce a connection prompt");
        };
        assert!(text.contains("taking over its existing feedback association"));
        assert!(agent.artifact_ticks(chat).await.unwrap().is_empty());
        server.abort();
        core.shutdown().await;
    }

    #[tokio::test]
    async fn list_removal_and_remote_delete_cancel_the_same_canonical_local_subscription() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let (origin, server, requests) = artifact_service().await;
        let client = ArtifactClient::for_origin(&root, &origin).unwrap();
        let device = client.start_device_login().await.unwrap();
        client
            .finish_device_login(device, &tokio_util::sync::CancellationToken::new())
            .await
            .unwrap();
        for remote_delete in [false, true] {
            let (agent, core, remote, chat) = subscribed_fixture(directory.path(), &origin).await;
            let mut tick = client
                .read("doc")
                .await
                .unwrap()
                .tick(&format!("{origin}/"), 1);
            tick.origin = format!("{origin}/");
            let console = Console::test_for_chat(chat);
            console
                .update_subscribed_ticks(chat, &remote.rows.lock().unwrap().clone())
                .unwrap();
            if !remote_delete {
                agent.add_artifact_tick(chat, tick.clone()).await.unwrap();
                console
                    .update_artifact_ticks(&agent.snapshot(chat).await.unwrap())
                    .unwrap();
            }
            let subscription = remote.rows.lock().unwrap()[0].id;
            if remote_delete {
                let artifact = client.read("doc").await.unwrap();
                apply_choice_with_client(
                    &agent,
                    &console,
                    &root,
                    Some(chat),
                    super::super::artifacts_picker::ArtifactChoice {
                        picker: Uuid::new_v4(),
                        artifact,
                        action: super::super::artifacts_picker::ArtifactPickerAction::Delete,
                    },
                    &client,
                )
                .await
                .unwrap();
            } else {
                tick_action(&agent, &console, Some(&root), chat, tick.instance_id(), 'X')
                    .await
                    .unwrap();
                assert!(
                    !requests
                        .lock()
                        .unwrap()
                        .iter()
                        .any(|request| request.starts_with("DELETE"))
                );
            }
            assert_eq!(*remote.cancelled.lock().unwrap(), vec![subscription]);
            assert!(agent.tick_subscriptions(chat).await.unwrap().is_empty());
            assert!(agent.artifact_ticks(chat).await.unwrap().is_empty());
            assert!(
                !console.tick_list_has_entries().unwrap(),
                "local cleanup left a cancelled subscription visible"
            );
            core.shutdown().await;
        }
        assert_eq!(
            requests
                .lock()
                .unwrap()
                .iter()
                .filter(|request| request.starts_with("DELETE"))
                .count(),
            1
        );
        server.abort();
    }

    #[tokio::test]
    async fn feedback_action_requires_a_successful_association_read() {
        let directory = tempfile::tempdir().unwrap();
        let core = crate::runtime::Agent::new_embedded(
            test_support::config(),
            Arc::new(test_support::ScriptedModel::new(Vec::new())),
        );
        let agent = ConsoleExecution::from_embedded(core.clone(), test_support::config());
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
        let result = tick_action(
            &agent,
            &console,
            Some(directory.path()),
            chat,
            tick.instance_id(),
            'f',
        )
        .await;
        assert!(
            result.is_err(),
            "failed association read must not produce a feedback prompt"
        );
        core.shutdown().await;
    }

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
