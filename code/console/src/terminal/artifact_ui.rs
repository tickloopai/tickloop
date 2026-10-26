use super::{console::Console, execution::ConsoleExecution};
use std::path::Path;
use tickloop_harness::artifacts::{ArtifactClient, ArtifactError, artifact_type};
use uuid::Uuid;

pub(super) async fn refresh(
    agent: &ConsoleExecution,
    console: &Console,
    root: Option<&Path>,
    chat: Uuid,
) -> Result<(), String> {
    let status = agent
        .status(chat)
        .await
        .map_err(|error| error.to_string())?;
    let artifact_ticks = status.artifact_ticks.clone();
    console.update_monitor_chat(status)?;
    let Some(root) = root else {
        return Ok(());
    };
    use futures_util::StreamExt;
    const ARTIFACT_REFRESH_CONCURRENCY: usize = 4;
    let sink = console.artifact_metadata_sink();
    let root = root.to_path_buf();
    tokio::spawn(async move {
        futures_util::stream::iter(artifact_ticks)
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
) -> Result<(), String> {
    if action == 'r' {
        refresh(agent, console, root, chat).await?;
        return Ok(());
    }
    if action == 'x' {
        return remove_local_artifact(agent, console, chat, &id).await;
    }
    let ticks = agent
        .artifact_ticks(chat)
        .await
        .map_err(|error| error.to_string())?;
    let Some(tick) = ticks.into_iter().find(|tick| tick.instance_id() == id) else {
        return Ok(());
    };
    match action {
        'o' => {
            if let Err(error) = super::login::open_artifact(&tick.url).await {
                console
                    .report_monitor_error(chat, &format!("{error} · press c to copy the link"))?;
            }
        }
        'c' => console.copy_artifact_link(&tick.url)?,
        'f' | 'h' => {
            agent
                .link_artifact(
                    chat,
                    tick,
                    if action == 'h' {
                        tickloop_harness::protocol::ArtifactFeedbackHistory::All
                    } else {
                        tickloop_harness::protocol::ArtifactFeedbackHistory::NewOnly
                    },
                )
                .await
                .map_err(|error| error.to_string())?;
        }
        _ => {}
    }
    Ok(())
}

async fn remove_local_artifact(
    agent: &ConsoleExecution,
    console: &Console,
    chat: Uuid,
    instance: &str,
) -> Result<(), String> {
    agent
        .remove_artifact_tick(chat, instance.to_owned())
        .await
        .map_err(|error| error.to_string())?;
    console.update_monitor_chat(
        agent
            .status(chat)
            .await
            .map_err(|error| error.to_string())?,
    )?;
    Ok(())
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
        .ok_or("Not signed in to Tickloop Artifacts")?;
    if !interactive {
        let page = client
            .list("all", "", None)
            .await
            .map_err(|error| error.to_string())?;
        for artifact in page.artifacts.iter().take(20) {
            let type_name = artifact_type(&artifact.format)
                .map_or(artifact.format.as_str(), |artifact_type| artifact_type.name);
            console.write_line(&super::safe_text(&format!(
                "{} · {type_name} · {} · {}",
                artifact.title, artifact.access_role, artifact.url
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
        tickloop_harness::artifacts::ArtifactPage,
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
        ArtifactPickerAction::Add(history) => {
            let chat = chat.ok_or("Select a Chat before adding an artifact")?;
            let tick = artifact.tick(
                client.origin(),
                tickloop_harness::base::private_fs::unix_time_milliseconds(),
            );
            let id = tick.instance_id();
            agent
                .link_artifact(chat, tick.clone(), history)
                .await
                .map_err(|error| error.to_string())?;
            agent
                .add_artifact_tick(chat, tick)
                .await
                .map_err(|error| error.to_string())?;
            console.update_monitor_chat(
                agent
                    .status(chat)
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
                let instance = tick_protocol::artifact_instance_id(
                    client.origin(),
                    &tickloop_harness::artifacts::canonical_artifact_id(&artifact.id),
                );
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
    use std::sync::{Arc, Mutex};
    use tickloop_code_daemon::EmbeddedAgentAccess;

    use tickloop_code_access::{
        AccessError, AgentAccess, AgentCommand, CommandEnvelope, CommandReceipt, CommandResult,
        DisplayReceipt, DisplayUpdate, LaunchSpec, Observation, ObservationRequest, ViewRef,
    };

    async fn artifact_service() -> (String, tokio::task::JoinHandle<()>, Arc<Mutex<Vec<String>>>) {
        artifact_service_with_id("doc").await
    }

    async fn artifact_service_with_id(
        id: &str,
    ) -> (String, tokio::task::JoinHandle<()>, Arc<Mutex<Vec<String>>>) {
        let id = id.to_owned();
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
                let body = if request
                    .starts_with("GET /.well-known/oauth-authorization-server/account ")
                {
                    serde_json::json!({"issuer":format!("{endpoint}/account"),"token_endpoint":format!("{endpoint}/account/token"),"device_authorization_endpoint":format!("{endpoint}/account/device"),"userinfo_endpoint":format!("{endpoint}/account/userinfo"),"revocation_endpoint":format!("{endpoint}/account/revoke"),"grant_types_supported":["urn:ietf:params:oauth:grant-type:device_code"]})
                } else if request.starts_with("GET /account/userinfo ") {
                    serde_json::json!({"sub":"owner","name":"Owner","email":"owner@example.test"})
                } else if request.starts_with("POST /account/device ") {
                    serde_json::json!({"device_code":"device","user_code":"CODE","verification_uri":format!("{endpoint}/account/device?code=CODE"),"expires_in":60,"interval":1})
                } else if request.starts_with("POST /account/token ") {
                    serde_json::json!({"access_token":"test-token","token_type":"Bearer","expires_in":600,"refresh_token":"refresh-test-token"})
                } else if request.starts_with(&format!("DELETE /artifact/api/v1/artifacts/{id} "))
                    || request.starts_with(&format!("PUT /artifact/api/v1/artifacts/{id}/pin "))
                    || request.starts_with(&format!("DELETE /artifact/api/v1/artifacts/{id}/pin "))
                    || request
                        .starts_with(&format!("POST /artifact/api/v1/artifacts/{id}/metadata "))
                {
                    let input: serde_json::Value =
                        serde_json::from_str(request.split("\r\n\r\n").nth(1).unwrap()).unwrap();
                    serde_json::json!({"operation_id":input["operation_id"],"status":"committed"})
                } else if request.starts_with(&format!("PUT /artifact/api/v1/agent/links/{id} ")) {
                    let input: serde_json::Value =
                        serde_json::from_str(request.split("\r\n\r\n").nth(1).unwrap()).unwrap();
                    serde_json::json!({"artifact_id":id,"binding_id":input["binding_id"],"epoch":2})
                } else {
                    serde_json::json!({"id":id,"title":"Shared document","format":"markdown","access_role":"owner","url":format!("{endpoint}/a/doc"),"agent_links":[{"binding_id":Uuid::new_v4(),"chat_id":Uuid::new_v4(),"loop_id":Uuid::new_v4(),"epoch":1}]})
                };
                let body = body.to_string();
                socket.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes()).await.unwrap();
            }
        });
        (origin, task, requests)
    }

    /// A Chat of an Agent that reaches the artifact service at `origin`.
    async fn artifact_fixture(
        directory: &Path,
        origin: &str,
    ) -> (ConsoleExecution, tickloop_harness::runtime::Agent, Uuid) {
        let core = tickloop_harness::runtime::Agent::new_embedded(
            tickloop_harness::test_support::config(),
            Arc::new(tickloop_harness::test_support::ScriptedModel::new(
                Vec::new(),
            )),
        );
        core.install_host_tools(vec![Arc::new(
            tickloop_harness::runtime::mcp_tools::McpHostToolProvider::new(
                ArtifactClient::for_origin(directory.canonicalize().unwrap(), origin).unwrap(),
                None,
            ),
        )
            as Arc<dyn tickloop_harness::runtime::host_tools::HostToolProvider>])
            .unwrap();
        let access = Arc::new(EmbeddedAgentAccess::new(core.clone()));
        let agent = ConsoleExecution::from_access_for_test(
            access.clone(),
            access.daemon_epoch(),
            ViewRef {
                session_id: Uuid::new_v4(),
                view_id: Uuid::new_v4(),
            },
            LaunchSpec::embedded(directory.into()),
            None,
        );
        let chat = agent.create_chat(directory.into()).await.unwrap();
        (agent, core, chat)
    }

    async fn sign_in(root: &Path, origin: &str) -> ArtifactClient {
        let client = ArtifactClient::for_origin(root, origin).unwrap();
        let device = client.start_device_login().await.unwrap();
        client
            .finish_device_login(device, &tokio_util::sync::CancellationToken::new())
            .await
            .unwrap();
        client
    }

    #[tokio::test]
    async fn open_copy_and_feedback_act_on_the_listed_artifact() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let (origin, server, requests) = artifact_service().await;
        let client = sign_in(&root, &origin).await;
        let (agent, core, chat) = artifact_fixture(directory.path(), &origin).await;
        let tick = client.read("doc").await.unwrap().tick(&origin, 1);
        let id = tick.instance_id();
        agent.add_artifact_tick(chat, tick).await.unwrap();
        let console = Console::test_for_chat(chat);
        tick_action(&agent, &console, Some(&root), chat, id.clone(), 'o')
            .await
            .unwrap();
        assert!(
            console
                .test_monitor_error()
                .unwrap()
                .contains("Only HTTPS artifact links can be opened")
        );
        tick_action(&agent, &console, Some(&root), chat, id.clone(), 'c')
            .await
            .unwrap();
        assert_eq!(
            console.test_clipboard_contents(),
            vec![format!("{origin}/a/doc")]
        );
        tick_action(&agent, &console, Some(&root), chat, id, 'f')
            .await
            .unwrap();
        assert!(
            requests
                .lock()
                .unwrap()
                .iter()
                .any(|request| request.starts_with("PUT /artifact/api/v1/agent/links/doc "))
        );
        assert!(console.test_status_text().is_empty());
        let snapshot = agent.snapshot(chat).await.unwrap();
        let binding = &snapshot.chat_loop.event_sources[0];
        assert_eq!((binding.artifact_id.as_str(), binding.epoch), ("doc", 2));
        server.abort();
        core.shutdown().await;
    }

    #[tokio::test]
    async fn picker_pin_rename_and_delete_work_with_uuid_and_base62_artifacts() {
        use super::super::artifacts_picker::{ArtifactChoice, ArtifactPickerAction};
        for id in [
            "e1b85a39-a57b-4c28-a8a5-ae9233aead3e",
            "6rvWPvOnhVABug7llbHb26",
        ] {
            let directory = tempfile::tempdir().unwrap();
            let root = directory.path().canonicalize().unwrap();
            let (origin, server, requests) = artifact_service_with_id(id).await;
            let client = sign_in(&root, &origin).await;
            let (agent, core, chat) = artifact_fixture(directory.path(), &origin).await;
            let artifact = client.read(id).await.unwrap();
            agent
                .add_artifact_tick(chat, artifact.tick(&origin, 1))
                .await
                .unwrap();
            let console = Console::test_for_chat(chat);
            console
                .update_monitor_chat(agent.status(chat).await.unwrap())
                .unwrap();
            for action in [
                ArtifactPickerAction::Pin(true),
                ArtifactPickerAction::Pin(false),
                ArtifactPickerAction::Rename("Renamed".into()),
                ArtifactPickerAction::Delete,
            ] {
                apply_choice_with_client(
                    &agent,
                    &console,
                    &root,
                    Some(chat),
                    ArtifactChoice {
                        picker: Uuid::new_v4(),
                        artifact: artifact.clone(),
                        action,
                    },
                    &client,
                )
                .await
                .unwrap();
            }
            assert!(agent.artifact_ticks(chat).await.unwrap().is_empty());
            assert!(!console.monitor_has_entries().unwrap());
            let requests = requests.lock().unwrap().clone();
            for (method, suffix) in [
                ("PUT", "/pin"),
                ("DELETE", "/pin"),
                ("POST", "/metadata"),
                ("DELETE", ""),
            ] {
                assert!(requests.iter().any(|request| request
                    == &format!("{method} /artifact/api/v1/artifacts/{id}{suffix} HTTP/1.1")));
            }
            core.shutdown().await;
            server.abort();
        }
    }

    #[tokio::test]
    async fn list_removal_and_remote_delete_take_out_the_same_canonical_local_artifact() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let (origin, server, requests) = artifact_service().await;
        let client = sign_in(&root, &origin).await;
        for remote_delete in [false, true] {
            let (agent, core, chat) = artifact_fixture(directory.path(), &origin).await;
            let mut tick = client
                .read("doc")
                .await
                .unwrap()
                .tick(&format!("{origin}/"), 1);
            tick.origin = format!("{origin}/");
            agent.add_artifact_tick(chat, tick.clone()).await.unwrap();
            let console = Console::test_for_chat(chat);
            console
                .update_monitor_chat(agent.status(chat).await.unwrap())
                .unwrap();
            assert!(console.monitor_has_entries().unwrap());
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
                tick_action(&agent, &console, Some(&root), chat, tick.instance_id(), 'x')
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
            assert!(agent.artifact_ticks(chat).await.unwrap().is_empty());
            assert!(
                !console.monitor_has_entries().unwrap(),
                "local cleanup left the artifact visible"
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
    async fn feedback_action_without_the_artifact_service_links_nothing() {
        let directory = tempfile::tempdir().unwrap();
        let core = tickloop_harness::runtime::Agent::new_embedded(
            tickloop_harness::test_support::config(),
            Arc::new(tickloop_harness::test_support::ScriptedModel::new(
                Vec::new(),
            )),
        );
        let agent =
            ConsoleExecution::from_embedded(core.clone(), tickloop_harness::test_support::config());
        let chat = agent.create_chat(directory.path().into()).await.unwrap();
        let tick = tickloop_harness::protocol::ArtifactTick {
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
            result.is_err_and(|error| error.contains("Tickloop Artifacts is unavailable")),
            "linking needs Tickloop Artifacts"
        );
        assert!(
            agent
                .snapshot(chat)
                .await
                .unwrap()
                .chat_loop
                .event_sources
                .is_empty()
        );
        core.shutdown().await;
    }

    struct FeedbackRemote {
        inner: EmbeddedAgentAccess,
        linked: Mutex<
            Vec<(
                tickloop_harness::protocol::ArtifactTick,
                tickloop_harness::protocol::ArtifactFeedbackHistory,
            )>,
        >,
    }
    #[async_trait::async_trait]
    impl AgentAccess for FeedbackRemote {
        async fn observe(&self, request: ObservationRequest) -> Result<Observation, AccessError> {
            self.inner.observe(request).await
        }
        async fn execute(&self, envelope: CommandEnvelope) -> Result<CommandReceipt, AccessError> {
            if let AgentCommand::LinkArtifact { tick, history, .. } = &envelope.command {
                self.linked.lock().unwrap().push((tick.clone(), *history));
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

    #[tokio::test]
    async fn feedback_key_links_the_artifact_directly() {
        let directory = tempfile::tempdir().unwrap();
        let model = Arc::new(tickloop_harness::test_support::ScriptedModel::new(
            Vec::new(),
        ));
        let runtime = tickloop_harness::runtime::Agent::new_embedded(
            tickloop_harness::test_support::config(),
            model.clone(),
        );
        let inner = EmbeddedAgentAccess::new(runtime.clone());
        let remote = Arc::new(FeedbackRemote {
            inner,
            linked: Mutex::new(Vec::new()),
        });
        let agent = ConsoleExecution::from_access_for_test(
            remote.clone(),
            remote.inner.daemon_epoch(),
            ViewRef {
                session_id: Uuid::new_v4(),
                view_id: Uuid::new_v4(),
            },
            LaunchSpec::embedded(directory.path().into()),
            None,
        );
        let chat = agent.create_chat(directory.path().into()).await.unwrap();
        let tick = tickloop_harness::protocol::ArtifactTick {
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
        tick_action(&agent, &console, None, chat, tick.instance_id(), 'f')
            .await
            .unwrap();
        assert_eq!(
            *remote.linked.lock().unwrap(),
            vec![(
                tick.clone(),
                tickloop_harness::protocol::ArtifactFeedbackHistory::NewOnly
            )]
        );
        tick_action(&agent, &console, None, chat, tick.instance_id(), 'h')
            .await
            .unwrap();
        assert_eq!(
            remote.linked.lock().unwrap()[1],
            (
                tick,
                tickloop_harness::protocol::ArtifactFeedbackHistory::All
            )
        );
        assert!(console.test_status_text().is_empty());
        assert!(model.requests.lock().unwrap().is_empty());
        runtime.shutdown().await;
    }

    #[tokio::test]
    async fn x_directly_unlinks_an_artifact_and_reports_it() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let (origin, server, _requests) = artifact_service().await;
        let client = sign_in(&root, &origin).await;
        let (agent, core, chat) = artifact_fixture(directory.path(), &origin).await;
        let tick = client.read("doc").await.unwrap().tick(&origin, 1);
        let id = tick.instance_id();
        agent.add_artifact_tick(chat, tick).await.unwrap();
        let console = Console::test_for_chat(chat);
        tick_action(&agent, &console, Some(&root), chat, id.clone(), 'f')
            .await
            .unwrap();

        tick_action(&agent, &console, Some(&root), chat, id.clone(), 'x')
            .await
            .unwrap();
        assert!(console.test_status_text().is_empty());
        let snapshot = agent.snapshot(chat).await.unwrap();
        assert!(snapshot.chat_loop.event_sources.is_empty());
        assert!(snapshot.artifact_ticks.is_empty());
        assert!(!console.monitor_has_entries().unwrap());
        refresh(&agent, &console, Some(&root), chat).await.unwrap();
        assert!(
            !console.monitor_has_entries().unwrap(),
            "an unlinked artifact stays out of Monitor"
        );
        server.abort();
        core.shutdown().await;
    }

    #[tokio::test]
    async fn x_removes_an_artifact_that_delivers_nothing_here_without_asking() {
        let directory = tempfile::tempdir().unwrap();
        let core = tickloop_harness::runtime::Agent::new_embedded(
            tickloop_harness::test_support::config(),
            Arc::new(tickloop_harness::test_support::ScriptedModel::new(
                Vec::new(),
            )),
        );
        let agent =
            ConsoleExecution::from_embedded(core.clone(), tickloop_harness::test_support::config());
        let chat = agent.create_chat(directory.path().into()).await.unwrap();
        let tick = tickloop_harness::protocol::ArtifactTick {
            artifact_id: "doc".into(),
            origin: "https://docs.example".into(),
            title: "Elsewhere".into(),
            format: "markdown".into(),
            url: "https://docs.example/a/doc".into(),
            access_role: "owner".into(),
            added_at_ms: 1,
        };
        agent.add_artifact_tick(chat, tick.clone()).await.unwrap();
        let console = Console::test_for_chat(chat);
        tick_action(&agent, &console, None, chat, tick.instance_id(), 'x')
            .await
            .unwrap();
        assert!(console.test_status_text().is_empty());
        assert!(agent.artifact_ticks(chat).await.unwrap().is_empty());
        core.shutdown().await;
    }
}
