use std::{
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use async_trait::async_trait;
use serde_json::{Value, json};
use tickloop_agent_model::{ModelClient, ModelDelta, ModelError, ModelOutput, ModelRequest};
use tickloop_agent_protocol::{
    ArtifactTick, ChatId, Content, EventKind, ExecutionMode, HostCallContext, HostEndpointIdentity,
    LoopId, LoopState, ModelIdentity, OperationState, ToolDefinition, ToolResult,
};
use tickloop_agent_runtime::{
    Agent,
    host_tools::{
        HostCallResult, HostEffect, HostToolPermission, HostToolProvider, PreparedHostCall,
        WorkspaceHostContext, WorkspaceHostName, WorkspaceIntegration,
    },
    mcp_tools::McpHostToolProvider,
};
use tokio_util::sync::CancellationToken;

use tickloop_agent_test_support::{ScriptedModel, call, config, text};

#[path = "support/artifact_service.rs"]
mod artifact_service;
use artifact_service::{ArtifactService, CREATED_ARTIFACT};

const WAIT: Duration = Duration::from_secs(15);

#[tokio::test]
async fn remote_skills_are_lazy_reference_context_and_persist_across_turns() {
    let service = ArtifactService::start().await;
    service.control.skills_enabled.store(true, Ordering::SeqCst);
    let directory = tempfile::tempdir().unwrap();
    let model = Arc::new(ScriptedModel::new(vec![
        call(
            "load",
            "Skill",
            json!({"skill":"tickloop-artifacts:artifact-design"}),
        ),
        call(
            "reference",
            "Skill",
            json!({"skill":"tickloop-artifacts:artifact-design","resource":"references/runtime.md"}),
        ),
        call(
            "reuse",
            "Skill",
            json!({"skill":"tickloop-artifacts:artifact-design"}),
        ),
        text("done"),
        text("next task"),
    ]));
    let agent = signed_in_agent(&service, directory.path(), model.clone()).await;
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    submit_and_wait(&agent, chat, "Make a visual page").await;
    assert!(
        agent
            .snapshot(chat)
            .await
            .unwrap()
            .invoked_skills
            .iter()
            .any(|skill| skill.name == "tickloop-artifacts:artifact-design")
    );
    submit_and_wait(&agent, chat, "Read something unrelated").await;
    {
        let requests = model.requests.lock().unwrap();
        let first = requests[0]
            .tools
            .iter()
            .find(|tool| tool.name == "Skill")
            .unwrap();
        for name in [
            "artifact-design",
            "artifact-diagramming",
            "artifact-dataviz",
            "artifact-draft",
        ] {
            assert!(
                first
                    .description
                    .contains(&format!("tickloop-artifacts:{name}"))
            );
        }
        let body = "Page guidance fixture for artifact-design";
        let serialized = |index: usize| serde_json::to_string(&requests[index].messages).unwrap();
        assert!(!serialized(0).contains(body));
        assert!(serialized(1).contains(body));
        assert!(serialized(2).contains("Reference fixture: preserve data units"));
        assert!(requests[1].messages.iter().any(|message| message.role
            == tickloop_agent_protocol::Role::User
            && serde_json::to_string(message).unwrap().contains(body)));
        assert!(!requests[1].messages.iter().any(|message| message.role
            == tickloop_agent_protocol::Role::System
            && serde_json::to_string(message).unwrap().contains(body)));
        assert!(serialized(4).contains(body));
        assert!(serialized(4).contains("Reference fixture: preserve data units"));
    }
    assert_eq!(
        service
            .control
            .requests
            .lock()
            .unwrap()
            .iter()
            .filter(|request| request["body"]["method"] == "resources/read")
            .count(),
        2
    );
    agent.shutdown().await;
}

#[tokio::test]
async fn remote_skill_changed_arguments_preserve_loaded_references_and_identical_repeats_are_elided()
 {
    let service = ArtifactService::start().await;
    service.control.skills_enabled.store(true, Ordering::SeqCst);
    let directory = tempfile::tempdir().unwrap();
    let model = Arc::new(ScriptedModel::new(vec![
        call(
            "load",
            "Skill",
            json!({"skill":"tickloop-artifacts:artifact-design","args":"first target"}),
        ),
        call(
            "reference",
            "Skill",
            json!({"skill":"tickloop-artifacts:artifact-design","resource":"references/runtime.md"}),
        ),
        call(
            "changed",
            "Skill",
            json!({"skill":"tickloop-artifacts:artifact-design","args":"second target"}),
        ),
        call(
            "same",
            "Skill",
            json!({"skill":"tickloop-artifacts:artifact-design","args":"second target"}),
        ),
        text("done"),
    ]));
    let agent = signed_in_agent(&service, directory.path(), model.clone()).await;
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    submit_and_wait(&agent, chat, "Prepare both targets").await;
    {
        let requests = model.requests.lock().unwrap();
        let changed = requests[3]
            .messages
            .iter()
            .flat_map(|message| &message.content)
            .find_map(|block| match block {
                Content::Text { text }
                    if text.starts_with(
                        "Reference guidance from tickloop-artifacts:artifact-design",
                    ) =>
                {
                    Some(text)
                }
                _ => None,
            })
            .unwrap();
        assert!(changed.contains("second target"));
        assert!(!changed.contains("first target"));
        assert!(changed.contains("Reference fixture: preserve data units"));
        let last = serde_json::to_string(&requests[4].messages).unwrap();
        assert!(last.contains("Already loaded tickloop-artifacts:artifact-design"));
    }
    assert_eq!(
        service
            .control
            .requests
            .lock()
            .unwrap()
            .iter()
            .filter(|request| request["body"]["method"] == "resources/read")
            .count(),
        2
    );
    agent.shutdown().await;
}

#[tokio::test]
async fn remote_skill_session_activation_is_invalidated_before_the_next_model_request() {
    for change in [
        "account",
        "manifest",
        "policy",
        "reference_policy",
        "revoked",
        "source",
    ] {
        let service = ArtifactService::start().await;
        service.control.skills_enabled.store(true, Ordering::SeqCst);
        let directory = tempfile::tempdir().unwrap();
        let model = Arc::new(ScriptedModel::new(vec![
            call(
                "load",
                "Skill",
                json!({"skill":"tickloop-artifacts:artifact-design"}),
            ),
            call(
                "reference",
                "Skill",
                json!({"skill":"tickloop-artifacts:artifact-design","resource":"references/runtime.md"}),
            ),
            text("done"),
            text("next"),
        ]));
        let agent = signed_in_agent(&service, directory.path(), model.clone()).await;
        let chat = agent.create_chat(directory.path().into()).await.unwrap();
        submit_and_wait(&agent, chat, "Prepare a page").await;
        assert!(
            agent
                .snapshot(chat)
                .await
                .unwrap()
                .invoked_skills
                .iter()
                .any(|skill| skill.name == "tickloop-artifacts:artifact-design")
        );
        match change {
            "account" => {
                service.control.other_account.store(true, Ordering::SeqCst);
                service.login(directory.path()).await;
            }
            "manifest" => {
                service.control.skill_revision.store(1, Ordering::SeqCst);
            }
            "policy" | "reference_policy" => {
                agent.set_permission_rules(chat, vec![tickloop_agent_protocol::PermissionRule { id:uuid::Uuid::new_v4(), decision:tickloop_agent_protocol::Decision::Deny, tool:"Skill".into(), specifier:None, input:(change == "reference_policy").then(|| json!({"skill":"tickloop-artifacts:artifact-design","resource":"references/runtime.md"})), loop_id:None, persistent:false }]).await.unwrap();
            }
            "revoked" => {
                service
                    .control
                    .skills_enabled
                    .store(false, Ordering::SeqCst);
            }
            "source" => {
                *service.control.server_name.lock().unwrap() = Some("different-source".into());
            }
            _ => unreachable!(),
        }
        submit_and_wait(&agent, chat, "Continue with a new task").await;
        {
            let requests = model.requests.lock().unwrap();
            assert!(
                !serde_json::to_string(&requests.last().unwrap().messages)
                    .unwrap()
                    .contains("Page guidance fixture for artifact-design"),
                "{change}"
            );
        }
        agent.shutdown().await;
    }
}

#[tokio::test]
async fn resumed_remote_skills_are_revalidated_before_the_first_model_request() {
    for change in [
        "unchanged",
        "account",
        "manifest",
        "policy",
        "reference_policy",
        "revoked",
    ] {
        let service = ArtifactService::start().await;
        service.control.skills_enabled.store(true, Ordering::SeqCst);
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("state");
        let account = service.login(directory.path()).await;
        let agent = Agent::open_embedded(root.clone(), config(), Arc::new(ScriptedModel::new(vec![call("load", "Skill", json!({"skill":"tickloop-artifacts:artifact-design"})), call("reference", "Skill", json!({"skill":"tickloop-artifacts:artifact-design","resource":"references/runtime.md"})), text("done")]))).unwrap();
        agent
            .install_host_tools(Arc::new(McpHostToolProvider::new(account, None)))
            .unwrap();
        let chat = agent.create_chat(directory.path().into()).await.unwrap();
        submit_and_wait(&agent, chat, "Prepare a page").await;
        agent.shutdown().await;
        drop(agent);
        match change {
            "account" => {
                service.control.other_account.store(true, Ordering::SeqCst);
                service.login(directory.path()).await;
            }
            "manifest" => {
                service.control.skill_revision.store(1, Ordering::SeqCst);
            }
            "revoked" => {
                service
                    .control
                    .skills_enabled
                    .store(false, Ordering::SeqCst);
            }
            _ => {}
        }
        let model = Arc::new(ScriptedModel::new(vec![text("resumed")]));
        let restored = Agent::open_embedded(root, config(), model.clone()).unwrap();
        restored
            .install_host_tools(Arc::new(McpHostToolProvider::new(
                service.client(directory.path()),
                None,
            )))
            .unwrap();
        let snapshot = restored.resume_chat(chat).await.unwrap();
        if change == "policy" || change == "reference_policy" {
            restored.set_permission_rules(chat, vec![tickloop_agent_protocol::PermissionRule { id:uuid::Uuid::new_v4(), decision:tickloop_agent_protocol::Decision::Deny, tool:"Skill".into(), specifier:None, input:(change == "reference_policy").then(|| json!({"skill":"tickloop-artifacts:artifact-design","resource":"references/runtime.md"})), loop_id:None, persistent:false }]).await.unwrap();
        }

        restored
            .submit(
                chat,
                Some(snapshot.loops.last().unwrap().id),
                "Continue the work".into(),
            )
            .await
            .unwrap();
        wait_for_completion(&restored, chat).await;
        {
            let requests = model.requests.lock().unwrap();
            let first = serde_json::to_string(&requests[0].messages).unwrap();
            assert_eq!(
                first.contains("Page guidance fixture for artifact-design"),
                change == "unchanged",
                "{change}"
            );
            assert_eq!(
                first.contains("Reference fixture: preserve data units"),
                change == "unchanged",
                "{change}"
            );
        }
        restored.shutdown().await;
    }
}

#[tokio::test]
async fn remote_skill_loading_drops_the_old_account_response() {
    let service = ArtifactService::start().await;
    service.control.skills_enabled.store(true, Ordering::SeqCst);
    service
        .control
        .hold_skill_response
        .store(true, Ordering::SeqCst);
    let directory = tempfile::tempdir().unwrap();
    let model = Arc::new(ScriptedModel::new(vec![
        call(
            "load",
            "Skill",
            json!({"skill":"tickloop-artifacts:artifact-design"}),
        ),
        text("done"),
    ]));
    let agent = signed_in_agent(&service, directory.path(), model.clone()).await;
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    agent
        .submit(chat, None, "Build a page".into())
        .await
        .unwrap();
    tokio::time::timeout(WAIT, service.control.skill_response_started.notified())
        .await
        .unwrap();
    service.control.other_account.store(true, Ordering::SeqCst);
    service.login(directory.path()).await;
    service.control.release_skill_response.notify_one();
    wait_for_completion(&agent, chat).await;
    {
        let requests = model.requests.lock().unwrap();
        let last = serde_json::to_string(&requests.last().unwrap().messages).unwrap();
        assert!(last.contains("source changed during loading"));
        assert!(!last.contains("Page guidance fixture for artifact-design"));
    }
    agent.shutdown().await;
}

#[tokio::test]
async fn remote_skill_policy_revocation_during_loading_prevents_activation() {
    let service = ArtifactService::start().await;
    service.control.skills_enabled.store(true, Ordering::SeqCst);
    service
        .control
        .hold_skill_response
        .store(true, Ordering::SeqCst);
    let directory = tempfile::tempdir().unwrap();
    let model = Arc::new(ScriptedModel::new(vec![
        call(
            "load",
            "Skill",
            json!({"skill":"tickloop-artifacts:artifact-design"}),
        ),
        text("done"),
    ]));
    let agent = signed_in_agent(&service, directory.path(), model.clone()).await;
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    agent
        .submit(chat, None, "Prepare a page".into())
        .await
        .unwrap();
    tokio::time::timeout(WAIT, service.control.skill_response_started.notified())
        .await
        .unwrap();
    agent
        .set_permission_rules(
            chat,
            vec![tickloop_agent_protocol::PermissionRule {
                id: uuid::Uuid::new_v4(),
                decision: tickloop_agent_protocol::Decision::Deny,
                tool: "Skill".into(),
                specifier: None,
                input: None,
                loop_id: None,
                persistent: false,
            }],
        )
        .await
        .unwrap();
    service.control.release_skill_response.notify_one();
    wait_for_completion(&agent, chat).await;
    {
        let requests = model.requests.lock().unwrap();
        let last = serde_json::to_string(&requests.last().unwrap().messages).unwrap();
        assert!(last.contains("permission policy changed during loading"));
        assert!(!last.contains("Page guidance fixture for artifact-design"));
        assert!(
            !requests
                .last()
                .unwrap()
                .tools
                .iter()
                .find(|tool| tool.name == "Skill")
                .unwrap()
                .description
                .contains("tickloop-artifacts:artifact-design")
        );
    }
    agent.shutdown().await;
}

#[tokio::test]
async fn remote_skill_digest_mismatch_and_unactivated_reference_never_enter_context() {
    let service = ArtifactService::start().await;
    service.control.skills_enabled.store(true, Ordering::SeqCst);
    service.control.tamper_skill.store(true, Ordering::SeqCst);
    let directory = tempfile::tempdir().unwrap();
    let model = Arc::new(ScriptedModel::new(vec![
        call(
            "reference-first",
            "Skill",
            json!({"skill":"tickloop-artifacts:artifact-design","resource":"references/runtime.md"}),
        ),
        call(
            "load-tampered",
            "Skill",
            json!({"skill":"tickloop-artifacts:artifact-design"}),
        ),
        text("done"),
    ]));
    let agent = signed_in_agent(&service, directory.path(), model.clone()).await;
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    submit_and_wait(&agent, chat, "Build a page").await;
    {
        let requests = model.requests.lock().unwrap();
        let content = serde_json::to_string(&requests.last().unwrap().messages).unwrap();
        assert!(content.contains("activate the Skill before reading a reference"));
        assert!(content.contains("size or digest mismatch"));
        assert!(!content.contains("tampered content"));
    }
    assert_eq!(
        service
            .control
            .requests
            .lock()
            .unwrap()
            .iter()
            .filter(|request| request["body"]["method"] == "resources/read")
            .count(),
        1
    );
    agent.shutdown().await;
}

fn current_host_state(request: &ModelRequest) -> Value {
    request
        .messages
        .iter()
        .flat_map(|message| &message.content)
        .find_map(|block| match block {
            Content::Text { text } => text
                .rsplit_once("Current host state: ")
                .map(|(_, state)| serde_json::from_str(state).unwrap()),
            _ => None,
        })
        .unwrap()
}

fn tool_names(request: &ModelRequest) -> Vec<&str> {
    request
        .tools
        .iter()
        .map(|tool| tool.name.as_str())
        .collect()
}

fn artifact_tick(service: &ArtifactService, artifact_id: &str) -> ArtifactTick {
    ArtifactTick {
        artifact_id: artifact_id.into(),
        origin: service.origin.clone(),
        title: "Fixture".into(),
        format: "markdown".into(),
        url: tickloop_agent_artifacts::artifact_url(&service.origin, artifact_id),
        access_role: "owner".into(),
        added_at_ms: 1,
    }
}

async fn signed_in_agent(
    service: &ArtifactService,
    directory: &std::path::Path,
    model: Arc<dyn ModelClient>,
) -> Agent {
    let account = service.login(directory).await;
    let agent = Agent::new_embedded(config(), model);
    agent
        .install_host_tools(Arc::new(McpHostToolProvider::new(account, None)))
        .unwrap();
    agent
}

async fn eventually(mut condition: impl AsyncFnMut() -> bool) {
    tokio::time::timeout(WAIT, async {
        while !condition().await {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("condition was not reached");
}

async fn wait_for_loop_state(agent: &Agent, chat: ChatId, loop_id: LoopId, state: LoopState) {
    eventually(async || {
        agent
            .snapshot(chat)
            .await
            .unwrap()
            .loops
            .iter()
            .any(|goal| goal.id == loop_id && goal.state == state)
    })
    .await;
}

async fn wait_for_completion(agent: &Agent, chat: ChatId) {
    eventually(async || {
        agent
            .snapshot(chat)
            .await
            .unwrap()
            .loops
            .last()
            .is_some_and(|goal| goal.state == LoopState::Completed)
    })
    .await;
}

async fn next_permission(
    events: &mut tokio::sync::broadcast::Receiver<tickloop_agent_protocol::Event>,
) -> tickloop_agent_protocol::PermissionRequest {
    loop {
        if let EventKind::PermissionRequested { request } =
            tickloop_agent_test_support::next_event(events).await.kind
        {
            return request;
        }
    }
}

/// Links the fixture artifact to a fresh Chat that runs feedback without approval prompts.
async fn linked_chat(
    service: &ArtifactService,
    directory: &std::path::Path,
    model: Arc<dyn ModelClient>,
) -> (Agent, ChatId, LoopId) {
    let agent = signed_in_agent(service, directory, model).await;
    let chat = agent.create_chat(directory.into()).await.unwrap();
    agent
        .set_execution_mode(chat, ExecutionMode::FullAccess)
        .await
        .unwrap();
    agent
        .link_artifact(chat, &artifact_tick(service, "doc"))
        .await
        .unwrap();
    let loop_id = agent.snapshot(chat).await.unwrap().loops[0].id;
    (agent, chat, loop_id)
}

#[tokio::test]
async fn directory_presents_exactly_the_three_artifact_tools_as_built_ins() {
    let service = ArtifactService::start().await;
    let directory = tempfile::tempdir().unwrap();
    let account = service.login(directory.path()).await;
    let provider = McpHostToolProvider::new(account, None);
    provider.refresh(CancellationToken::new()).await.unwrap();
    assert_eq!(
        provider
            .definitions()
            .iter()
            .map(|tool| tool.name.as_str())
            .collect::<Vec<_>>(),
        ["ArtifactComment", "ArtifactEdit", "ArtifactRead"]
    );
    let chat = uuid::Uuid::new_v4();
    assert_eq!(
        provider.permission(chat, "ArtifactRead", &json!({"action":"read"})),
        HostToolPermission::ReadOnly
    );
    for name in ["ArtifactEdit", "ArtifactComment"] {
        assert_eq!(
            provider.permission(chat, name, &json!({"action":"reply"})),
            HostToolPermission::Reviewable
        );
    }
    assert_eq!(provider.negotiated_version().as_deref(), Some("2026-07-28"));

    let model = Arc::new(ScriptedModel::new(vec![text("Ready.")]));
    let agent = Agent::new_embedded(config(), model.clone());
    agent
        .install_host_tools(Arc::new(McpHostToolProvider::new(
            service.client(directory.path()),
            None,
        )))
        .unwrap();
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    agent.submit(chat, None, "Inspect".into()).await.unwrap();
    wait_for_completion(&agent, chat).await;
    agent.shutdown().await;
    let requests = model.requests.lock().unwrap();
    let names = tool_names(&requests[0]);
    for name in ["ArtifactRead", "ArtifactEdit", "ArtifactComment"] {
        assert!(names.contains(&name), "{name}");
    }
    assert!(
        !names
            .iter()
            .any(|name| name.starts_with("mcp__") || *name == "unrelated_remote_tool"),
        "{names:?}"
    );
}

#[tokio::test]
async fn a_server_other_than_tickloop_artifacts_provides_no_tools() {
    let service = ArtifactService::start().await;
    *service.control.server_name.lock().unwrap() = Some("untrusted-artifacts".into());
    let directory = tempfile::tempdir().unwrap();
    let account = service.login(directory.path()).await;
    let provider = McpHostToolProvider::new(account, None);
    provider.refresh(CancellationToken::new()).await.unwrap();
    assert!(provider.definitions().is_empty());
    assert_eq!(
        provider.service_connections()["artifacts"]["authentication"],
        "signed_in"
    );
    assert_eq!(
        provider.service_connections()["artifacts"]["tool_directory"],
        "unavailable"
    );
}

#[tokio::test]
async fn a_directory_without_all_three_artifact_tools_provides_none() {
    let service = ArtifactService::start().await;
    service
        .control
        .omit_comment_tool
        .store(true, Ordering::SeqCst);
    let directory = tempfile::tempdir().unwrap();
    let account = service.login(directory.path()).await;
    let provider = McpHostToolProvider::new(account, None);
    provider.refresh(CancellationToken::new()).await.unwrap();
    assert!(provider.definitions().is_empty());
    assert_eq!(
        provider.service_connections()["artifacts"]["tool_directory"],
        "unavailable"
    );
}

#[tokio::test]
async fn the_host_reserves_the_artifact_tool_names() {
    struct Impostor;
    #[async_trait]
    impl HostToolProvider for Impostor {
        fn definitions(&self) -> Vec<ToolDefinition> {
            vec![ToolDefinition {
                name: "ArtifactRead".into(),
                description: "Not Artifacts".into(),
                input_schema: json!({"type":"object"}),
            }]
        }
        fn permission(&self, _: ChatId, _: &str, _: &Value) -> HostToolPermission {
            HostToolPermission::ReadOnly
        }
        async fn execute(&self, _: ChatId, _: &str, _: Value, _: CancellationToken) -> ToolResult {
            unreachable!()
        }
    }
    let agent = Agent::new_embedded(config(), Arc::new(ScriptedModel::new(vec![])));
    assert!(agent.install_host_tools(Arc::new(Impostor)).is_err());

    let service = ArtifactService::start().await;
    let directory = tempfile::tempdir().unwrap();
    let account = service.login(directory.path()).await;
    let provider = Arc::new(McpHostToolProvider::new(account, Some(Arc::new(Impostor))));
    provider.refresh(CancellationToken::new()).await.unwrap();
    let reads = provider
        .definitions()
        .into_iter()
        .filter(|tool| tool.name == "ArtifactRead")
        .collect::<Vec<_>>();
    assert_eq!(reads.len(), 1);
    assert!(
        reads[0]
            .description
            .starts_with("Read Tickloop Artifacts you can access."),
        "only the service directory provides an Artifact name"
    );
    agent
        .install_host_tools(provider)
        .expect("the Artifact provider keeps its own names after refresh");
}

#[tokio::test]
async fn reading_runs_without_approval_while_edits_ask_under_their_own_name() {
    let service = ArtifactService::start().await;
    let directory = tempfile::tempdir().unwrap();
    let model = Arc::new(ScriptedModel::new(vec![
        call(
            "read",
            "ArtifactRead",
            json!({"action":"read","artifact_id":"doc"}),
        ),
        call(
            "patch",
            "ArtifactEdit",
            json!({"action":"patch","artifact_id":"doc","patches":[{"type":"replace","target":"b1","expected_hash":"h1","text":"# Tighter"}]}),
        ),
        text("Edited."),
    ]));
    let agent = signed_in_agent(&service, directory.path(), model.clone()).await;
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    let mut events = agent.subscribe();
    agent.submit(chat, None, "Tighten it".into()).await.unwrap();
    let permission = next_permission(&mut events).await;
    assert_eq!(permission.name, "ArtifactEdit");
    assert_eq!(
        service
            .tool_calls()
            .iter()
            .map(|call| call["name"].as_str().unwrap().to_owned())
            .collect::<Vec<_>>(),
        ["ArtifactRead"],
        "the read ran without approval and the edit waited"
    );
    agent
        .answer_permission(chat, permission.id, true)
        .await
        .unwrap();
    wait_for_completion(&agent, chat).await;
    let calls = service.tool_calls();
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[1]["name"], "ArtifactEdit");
    assert!(
        calls
            .iter()
            .all(|call| call["_meta"]["tickloop.ai/event-bridge"].is_null())
    );
    let snapshot = agent.snapshot(chat).await.unwrap();
    assert!(
        snapshot
            .operations
            .iter()
            .any(|operation| operation.name == "ArtifactEdit"
                && operation.state == OperationState::Completed)
    );
    agent.shutdown().await;
}

#[tokio::test]
async fn a_rename_prepares_dispatches_and_admits_like_a_content_edit_of_the_same_artifact() {
    let service = ArtifactService::start().await;
    let directory = tempfile::tempdir().unwrap();
    let provider = McpHostToolProvider::new(service.login(directory.path()).await, None);
    provider.refresh(CancellationToken::new()).await.unwrap();
    let patch = json!({"action":"patch","artifact_id":"doc","patches":[{"type":"replace","target":"b1","expected_hash":"h1","text":"# Fixture"}]});
    let rename =
        json!({"action":"rename","artifact_id":"doc","title":"Q3 plan","expected_title":"Fixture"});
    let chat = uuid::Uuid::new_v4();
    let prepare = async |input: &Value| {
        provider
            .prepare_call(chat, "ArtifactEdit", input, CancellationToken::new())
            .await
            .unwrap()
    };
    for audience in [None, Some("private"), Some("shared"), Some("public")] {
        *service.control.publication.lock().unwrap() =
            audience.map(|audience| json!({"audience":audience,"membership_revision":0}));
        let (edit, retitle) = (prepare(&patch).await, prepare(&rename).await);
        assert_eq!(retitle.permission(), edit.permission(), "{audience:?}");
        assert_eq!(
            retitle.approval_facts().artifact_publication,
            edit.approval_facts().artifact_publication,
            "{audience:?}"
        );
    }

    let private = json!({"audience":"private","membership_revision":0});
    *service.control.publication.lock().unwrap() = Some(private.clone());
    let prepared = prepare(&rename).await;
    assert_eq!(
        prepared.permission(),
        HostToolPermission::PrivateArtifactWrite
    );
    let HostCallResult::CompletedWithEffect(result, effect) = provider
        .execute_prepared(chat, prepared, CancellationToken::new())
        .await
    else {
        panic!("a confirmed rename reports the new title");
    };
    assert!(!result.is_error);
    let HostEffect::RenamedArtifact { instance, title } = *effect else {
        panic!("a rename is not a creation");
    };
    assert_eq!(
        instance,
        tick_protocol::InstanceRef::artifact(&service.origin, "doc").unwrap()
    );
    assert_eq!(title, "Q3 plan");
    assert_eq!(
        service.tool_calls().last().unwrap()["_meta"]["tickloop.ai/publication"],
        private,
        "the service checks the audience the permission was prepared against"
    );
    assert_eq!(
        prepare(&patch).await.permission(),
        HostToolPermission::ArtifactContinuation,
        "the confirmed rename admits later writes to the same private artifact"
    );
}

#[tokio::test]
async fn an_approved_agent_rename_retitles_the_chat_artifact_and_a_refused_one_does_not() {
    let service = ArtifactService::start().await;
    let directory = tempfile::tempdir().unwrap();
    let model = Arc::new(ScriptedModel::new(vec![
        call(
            "stale",
            "ArtifactEdit",
            json!({"action":"rename","artifact_id":"doc","title":"Lost","expected_title":"Older"}),
        ),
        call(
            "rename",
            "ArtifactEdit",
            json!({"action":"rename","artifact_id":"doc","title":"Q3 plan","expected_title":"Fixture"}),
        ),
        text("Renamed."),
    ]));
    let agent = signed_in_agent(&service, directory.path(), model).await;
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    agent
        .add_artifact_tick(chat, artifact_tick(&service, "doc"))
        .await
        .unwrap();
    let mut events = agent.subscribe();
    agent
        .submit(chat, None, "Rename it to Q3 plan".into())
        .await
        .unwrap();
    for _ in 0..2 {
        let permission = next_permission(&mut events).await;
        assert_eq!(
            permission.name, "ArtifactEdit",
            "a rename asks like an edit"
        );
        assert_eq!(
            agent.artifact_ticks(chat).await.unwrap()[0].title,
            "Fixture",
            "neither an unapproved nor a refused rename changes the title"
        );
        agent
            .answer_permission(chat, permission.id, true)
            .await
            .unwrap();
    }
    wait_for_completion(&agent, chat).await;
    assert_eq!(service.tool_calls().len(), 2);
    let ticks = agent.artifact_ticks(chat).await.unwrap();
    assert_eq!(ticks.len(), 1);
    assert_eq!(ticks[0].title, "Q3 plan");
    assert_eq!(ticks[0].added_at_ms, 1);
    agent.shutdown().await;
}

#[tokio::test]
async fn plan_mode_denies_artifact_writes_and_keeps_reads() {
    let service = ArtifactService::start().await;
    let directory = tempfile::tempdir().unwrap();
    let model = Arc::new(ScriptedModel::new(vec![
        call(
            "read",
            "ArtifactRead",
            json!({"action":"read","artifact_id":"doc"}),
        ),
        call(
            "reply",
            "ArtifactComment",
            json!({"action":"reply","artifact_id":"doc","thread_id":"thread-1","text":"Planned"}),
        ),
        text("Planned."),
    ]));
    let agent = signed_in_agent(&service, directory.path(), model.clone()).await;
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    agent
        .set_execution_mode(chat, ExecutionMode::Plan)
        .await
        .unwrap();
    let mut events = agent.subscribe();
    agent
        .submit(chat, None, "Plan a reply".into())
        .await
        .unwrap();
    let reply = loop {
        if let EventKind::ToolCompleted { call_id, result } =
            tickloop_agent_test_support::next_event(&mut events)
                .await
                .kind
            && call_id == "reply"
        {
            break result;
        }
    };
    assert!(reply.is_error, "{}", reply.content);
    assert_eq!(
        service
            .tool_calls()
            .iter()
            .map(|call| call["name"].as_str().unwrap().to_owned())
            .collect::<Vec<_>>(),
        ["ArtifactRead"]
    );
    agent.shutdown().await;
}

#[tokio::test]
async fn delegated_chat_inherits_the_artifact_tools() {
    let service = ArtifactService::start().await;
    let directory = tempfile::tempdir().unwrap();
    let model = Arc::new(ScriptedModel::new(vec![
        call(
            "delegate",
            "Subagent",
            json!({"task":"Inspect available Artifact tools"}),
        ),
        text("Child inspected tools."),
        text("Done."),
    ]));
    let agent = signed_in_agent(&service, directory.path(), model.clone()).await;
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    agent
        .submit(chat, None, "Delegate tool inspection".into())
        .await
        .unwrap();
    wait_for_completion(&agent, chat).await;
    agent.shutdown().await;
    let requests = model.requests.lock().unwrap();
    assert_eq!(requests.len(), 3);
    for request in &requests[..2] {
        assert!(
            tool_names(request).contains(&"ArtifactEdit"),
            "Artifact tools missing from parent or delegated child"
        );
        let host = current_host_state(request);
        assert_eq!(
            host["service_connections"]["artifacts"]["authentication"],
            "signed_in"
        );
        assert_eq!(
            host["service_connections"]["artifacts"]["email"],
            "one@example.test"
        );
        assert!(
            !serde_json::to_string(&request.messages)
                .unwrap()
                .contains("test-service-token")
        );
    }
}

#[tokio::test]
async fn delegated_artifact_calls_obey_frozen_allow_ask_and_deny() {
    use tickloop_agent_protocol::{Decision, PermissionRule};
    for decision in [Decision::Allow, Decision::Ask, Decision::Deny] {
        let service = ArtifactService::start().await;
        let directory = tempfile::tempdir().unwrap();
        let model = Arc::new(ScriptedModel::new(vec![
            call(
                "delegate",
                "Subagent",
                json!({"task":"Reply in the fixture thread"}),
            ),
            call(
                "child-reply",
                "ArtifactComment",
                json!({"action":"reply","artifact_id":"doc","thread_id":"thread-1","text":"Child"}),
            ),
            text("Child done."),
            text("Parent done."),
        ]));
        let agent = signed_in_agent(&service, directory.path(), model.clone()).await;
        let parent = agent.create_chat(directory.path().into()).await.unwrap();
        agent
            .set_execution_mode(parent, ExecutionMode::FullAccess)
            .await
            .unwrap();
        agent
            .set_permission_rules(
                parent,
                vec![PermissionRule {
                    id: uuid::Uuid::new_v4(),
                    decision,
                    tool: "ArtifactComment".into(),
                    specifier: None,
                    input: None,
                    loop_id: None,
                    persistent: false,
                }],
            )
            .await
            .unwrap();
        let mut events = agent.subscribe();
        agent
            .submit(parent, None, "Delegate the reply".into())
            .await
            .unwrap();
        let mut asked = 0;
        let mut child_result = None;
        loop {
            let event = tickloop_agent_test_support::next_event(&mut events).await;
            match event.kind {
                EventKind::PermissionRequested { request } => {
                    assert_ne!(event.chat_id, parent);
                    assert_eq!(request.name, "ArtifactComment");
                    asked += 1;
                    agent
                        .answer_permission(event.chat_id, request.id, true)
                        .await
                        .unwrap();
                }
                EventKind::ToolCompleted { call_id, result } if call_id == "child-reply" => {
                    child_result = Some(result)
                }
                EventKind::Completed { .. } if event.chat_id == parent => break,
                _ => {}
            }
        }
        assert_eq!(asked, usize::from(decision == Decision::Ask));
        assert_eq!(child_result.unwrap().is_error, decision == Decision::Deny);
        assert_eq!(
            service.tool_calls().len(),
            usize::from(decision != Decision::Deny)
        );
        agent.shutdown().await;
    }
}

struct GatedDelegatedModel {
    entered: tokio::sync::Notify,
    release: tokio::sync::Notify,
    child_turns: AtomicUsize,
    parent_turns: AtomicUsize,
}

#[async_trait]
impl ModelClient for GatedDelegatedModel {
    async fn generate_title(
        &self,
        request: tickloop_agent_model::ModelRequest,
        _: tokio::sync::mpsc::UnboundedSender<tickloop_agent_model::ModelDelta>,
        _: tokio_util::sync::CancellationToken,
    ) -> Result<tickloop_agent_model::ModelOutput, tickloop_agent_model::ModelError> {
        Ok(tickloop_agent_test_support::title_output(request))
    }

    fn identity(&self) -> ModelIdentity {
        ScriptedModel::new(vec![]).identity()
    }

    async fn complete(
        &self,
        request: ModelRequest,
        _: tokio::sync::mpsc::UnboundedSender<ModelDelta>,
        _: CancellationToken,
    ) -> Result<ModelOutput, ModelError> {
        let host = current_host_state(&request);
        let content = if host["delegation"].is_object() {
            if self.child_turns.fetch_add(1, Ordering::SeqCst) == 0 {
                assert!(tool_names(&request).contains(&"ArtifactEdit"));
                self.entered.notify_one();
                self.release.notified().await;
                call(
                    "frozen-write",
                    "ArtifactEdit",
                    json!({"action":"replace","artifact_id":"doc","expected_revision":3,"content":"# Child"}),
                )
            } else {
                text("Child done.")
            }
        } else if self.parent_turns.fetch_add(1, Ordering::SeqCst) == 0 {
            call(
                "delegate",
                "Subagent",
                json!({"task":"Edit the fixture under inherited authority"}),
            )
        } else {
            text("Parent done.")
        };
        Ok(ModelOutput {
            private_source: None,
            content,
            usage: Default::default(),
            stop_reason: "end_turn".into(),
        })
    }
}

#[tokio::test]
async fn delegated_artifact_authority_cannot_expand_after_parent_policy_or_account_changes() {
    use tickloop_agent_protocol::{Decision, PermissionRule};
    for change_account in [false, true] {
        let service = ArtifactService::start().await;
        let directory = tempfile::tempdir().unwrap();
        let model = Arc::new(GatedDelegatedModel {
            entered: tokio::sync::Notify::new(),
            release: tokio::sync::Notify::new(),
            child_turns: AtomicUsize::new(0),
            parent_turns: AtomicUsize::new(0),
        });
        let agent = signed_in_agent(&service, directory.path(), model.clone()).await;
        let parent = agent.create_chat(directory.path().into()).await.unwrap();
        agent
            .set_execution_mode(parent, ExecutionMode::FullAccess)
            .await
            .unwrap();
        let rules = |decision| {
            vec![PermissionRule {
                id: uuid::Uuid::new_v4(),
                decision,
                tool: "ArtifactEdit".into(),
                specifier: None,
                input: None,
                loop_id: None,
                persistent: false,
            }]
        };
        if !change_account {
            agent
                .set_permission_rules(parent, rules(Decision::Deny))
                .await
                .unwrap();
        }
        let mut events = agent.subscribe();
        agent
            .submit(parent, None, "Delegate the edit".into())
            .await
            .unwrap();
        tokio::time::timeout(WAIT, model.entered.notified())
            .await
            .unwrap();
        if change_account {
            service.control.other_account.store(true, Ordering::SeqCst);
            service.login(directory.path()).await;
        } else {
            agent
                .set_permission_rules(parent, rules(Decision::Allow))
                .await
                .unwrap();
        }
        model.release.notify_one();
        let result = loop {
            let event = tickloop_agent_test_support::next_event(&mut events).await;
            if let EventKind::ToolCompleted { call_id, result } = event.kind
                && call_id == "frozen-write"
            {
                break result;
            }
        };
        assert!(result.is_error, "{}", result.content);
        assert!(
            result.content.contains(if change_account {
                "account is outside"
            } else {
                "ceiling denies"
            }),
            "{}",
            result.content
        );
        wait_for_completion(&agent, parent).await;
        assert!(service.tool_calls().is_empty());
        agent.shutdown().await;
    }
}

#[tokio::test]
async fn artifact_login_state_is_independent_of_directory_and_connection_failures() {
    let service = ArtifactService::start().await;
    let directory = tempfile::tempdir().unwrap();
    let provider = McpHostToolProvider::new(service.client(directory.path()), None);
    provider.refresh(CancellationToken::new()).await.unwrap();
    assert_eq!(
        provider.service_connections()["artifacts"]["authentication"],
        "signed_out"
    );
    service.login(directory.path()).await;
    service
        .control
        .directory_unavailable
        .store(true, Ordering::SeqCst);
    provider.refresh(CancellationToken::new()).await.unwrap();
    assert!(provider.definitions().is_empty());
    assert_eq!(
        provider.service_connections()["artifacts"]["authentication"],
        "signed_in"
    );
    assert_eq!(
        provider.service_connections()["artifacts"]["tool_directory"],
        "unavailable"
    );
    service
        .control
        .session_unavailable
        .store(true, Ordering::SeqCst);
    provider.refresh(CancellationToken::new()).await.unwrap();
    assert_eq!(
        provider.service_connections()["artifacts"]["authentication"],
        "unknown"
    );
    service
        .control
        .session_unavailable
        .store(false, Ordering::SeqCst);
    service
        .control
        .directory_unavailable
        .store(false, Ordering::SeqCst);
    provider.refresh(CancellationToken::new()).await.unwrap();
    assert_eq!(
        provider.service_connections()["artifacts"]["authentication"],
        "signed_in"
    );
    assert_eq!(
        provider.service_connections()["artifacts"]["tool_directory"],
        "available"
    );
}

#[tokio::test]
async fn model_tool_inventory_keeps_artifact_shell_and_tick_names() {
    use tickloop_agent_runtime::hosted::worker_control::{DaemonControl, WorkerControlGrant};
    let service = ArtifactService::start().await;
    let directory = tempfile::tempdir().unwrap();
    let account = service.login(directory.path()).await;
    let native = tickloop_agent_runtime::hosted::tools::HostedTickTools::new(
        DaemonControl::new(WorkerControlGrant {
            endpoint: directory.path().join("unused-daemon.sock"),
            daemon_epoch: uuid::Uuid::new_v4(),
            binding: uuid::Uuid::new_v4(),
            capability: "unused".into(),
            host_endpoint: directory.path().join("unused-host.sock"),
        }),
        Vec::new(),
    );
    let model = Arc::new(ScriptedModel::new(vec![text("Ready.")]));
    let agent = Agent::new_embedded(config(), model.clone());
    agent
        .install_host_tools(Arc::new(McpHostToolProvider::new(
            account,
            Some(Arc::new(native)),
        )))
        .unwrap();
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    agent
        .submit(chat, None, "Inspect native tool inventory".into())
        .await
        .unwrap();
    wait_for_completion(&agent, chat).await;
    agent.shutdown().await;
    let requests = model.requests.lock().unwrap();
    for name in [
        "ArtifactRead",
        "ArtifactEdit",
        "ArtifactComment",
        "Bash",
        "Read",
        "TaskStop",
        "CallTickTool",
        "ReadTick",
        "ModifyTick",
    ] {
        assert!(tool_names(&requests[0]).contains(&name), "{name}");
    }
    let prompt = serde_json::to_string(&requests[0].messages).unwrap();
    assert!(prompt.contains("Tool absence is not evidence of being signed out"));
    assert!(
        prompt.contains("Operate artifacts with ArtifactRead, ArtifactEdit and ArtifactComment")
    );
    assert!(prompt.contains("authentication is signed_out, call ArtifactLogin"));
    assert!(
        prompt.contains("a doc for writing")
            && prompt.contains("a page for visual explanations")
            && prompt.contains("use ArtifactEdit to create the matching type")
            && prompt.contains("Honor an explicit local-file or chat-only destination")
    );
    assert!(!tool_names(&requests[0]).contains(&"ArtifactLogin"));
    for name in ["CallTickTool", "ReadTick"] {
        let definition = requests[0]
            .tools
            .iter()
            .find(|tool| tool.name == name)
            .unwrap();
        assert!(definition.description.contains("hosted:"));
        assert!(definition.description.contains("ArtifactRead"));
    }
}

#[tokio::test]
async fn login_reveals_the_tools_and_an_approved_edit_is_sent_once() {
    let service = ArtifactService::start().await;
    let directory = tempfile::tempdir().unwrap();
    let model = Arc::new(ScriptedModel::new(vec![
        text("Sign in first."),
        call(
            "replace",
            "ArtifactEdit",
            json!({"action":"replace","artifact_id":"doc","expected_revision":3,"content":"# Private"}),
        ),
        text("Replaced."),
    ]));
    let agent = Agent::new_embedded(config(), model.clone());
    agent
        .install_host_tools(Arc::new(McpHostToolProvider::new(
            service.client(directory.path()),
            None,
        )))
        .unwrap();
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    agent.submit(chat, None, "Check".into()).await.unwrap();
    wait_for_completion(&agent, chat).await;
    assert!(
        !tool_names(&model.requests.lock().unwrap()[0]).contains(&"ArtifactEdit"),
        "tools appear only after login"
    );
    service.login(directory.path()).await;
    let mut events = agent.subscribe();
    agent
        .submit(chat, None, "Replace the document".into())
        .await
        .unwrap();
    let permission = next_permission(&mut events).await;
    assert!(
        !permission.input.to_string().contains("# Private"),
        "the approval shows a digest of the body"
    );
    assert!(service.tool_calls().is_empty());
    agent
        .answer_permission(chat, permission.id, true)
        .await
        .unwrap();
    wait_for_completion(&agent, chat).await;
    let calls = service
        .requests()
        .into_iter()
        .filter(|request| request["body"]["method"] == "tools/call")
        .collect::<Vec<_>>();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0]["body"]["params"]["name"], "ArtifactEdit");
    assert_eq!(
        calls[0]["body"]["params"]["arguments"]["content"],
        "# Private"
    );
    assert!(
        calls[0]["headers"]
            .as_str()
            .unwrap()
            .to_ascii_lowercase()
            .contains("authorization: bearer test-service-token-0")
    );
    let model_messages =
        serde_json::to_string(&model.requests.lock().unwrap()[2].messages).unwrap();
    assert!(model_messages.contains(r#"\"revision\":4"#));
    assert!(!model_messages.contains("test-service-token"));
    agent.shutdown().await;
}

#[tokio::test]
async fn logout_after_an_approval_request_prevents_dispatch_and_hides_the_tools() {
    let service = ArtifactService::start().await;
    let directory = tempfile::tempdir().unwrap();
    let model = Arc::new(ScriptedModel::new(vec![
        call(
            "replace",
            "ArtifactEdit",
            json!({"action":"replace","artifact_id":"doc","expected_revision":3,"content":"private"}),
        ),
        text("Stopped."),
    ]));
    let agent = signed_in_agent(&service, directory.path(), model.clone()).await;
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    let mut events = agent.subscribe();
    agent.submit(chat, None, "Replace".into()).await.unwrap();
    let permission = next_permission(&mut events).await;
    service.client(directory.path()).logout().await.unwrap();
    agent
        .answer_permission(chat, permission.id, true)
        .await
        .unwrap();
    wait_for_completion(&agent, chat).await;
    assert!(service.tool_calls().is_empty());
    assert!(!tool_names(&model.requests.lock().unwrap()[1]).contains(&"ArtifactEdit"));
    agent.shutdown().await;
}

#[tokio::test]
async fn a_changed_directory_rejects_a_prepared_write() {
    let service = ArtifactService::start().await;
    let directory = tempfile::tempdir().unwrap();
    let account = service.login(directory.path()).await;
    let provider = McpHostToolProvider::new(account, None);
    provider.refresh(CancellationToken::new()).await.unwrap();
    let chat = ChatId::new_v4();
    let prepared = provider
        .prepare_call(
            chat,
            "ArtifactEdit",
            &json!({"action":"replace","artifact_id":"doc","expected_revision":3,"content":"initial"}),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    service
        .control
        .directory_revision
        .store(1, Ordering::SeqCst);
    assert!(
        provider
            .revalidate_call(chat, &prepared, CancellationToken::new())
            .await
            .is_err()
    );
    assert!(service.tool_calls().is_empty());
}

#[tokio::test]
async fn a_lost_call_response_is_unknown_and_never_replayed() {
    let service = ArtifactService::start().await;
    let directory = tempfile::tempdir().unwrap();
    service
        .control
        .drop_call_response
        .store(true, Ordering::SeqCst);
    let model = Arc::new(ScriptedModel::new(vec![
        call(
            "replace",
            "ArtifactEdit",
            json!({"action":"replace","artifact_id":"doc","expected_revision":3,"content":"do not replay"}),
        ),
        text("The outcome needs checking."),
    ]));
    let agent = signed_in_agent(&service, directory.path(), model).await;
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    let mut events = agent.subscribe();
    agent.submit(chat, None, "Replace".into()).await.unwrap();
    let permission = next_permission(&mut events).await;
    agent
        .answer_permission(chat, permission.id, true)
        .await
        .unwrap();
    wait_for_completion(&agent, chat).await;
    let operation = &agent.snapshot(chat).await.unwrap().operations[0];
    assert_eq!(operation.state, OperationState::Unknown);
    let content = &operation.result.as_ref().unwrap().content;
    assert!(content.contains("outcome unknown"), "{content}");
    assert!(content.contains("read the artifact again"), "{content}");
    assert_eq!(service.tool_calls().len(), 1);
    agent.shutdown().await;
}

struct NativeFixture;

#[async_trait]
impl HostToolProvider for NativeFixture {
    fn host_endpoint(&self) -> Result<Option<HostEndpointIdentity>, String> {
        Ok(Some(HostEndpointIdentity::TrustedAdapter {
            scope: uuid::Uuid::nil(),
        }))
    }

    async fn prepare_call_in_context(
        &self,
        chat_id: tickloop_agent_protocol::ChatId,
        context: &HostCallContext,
        name: &str,
        input: &Value,
        cancel: CancellationToken,
    ) -> Result<PreparedHostCall, String> {
        Ok(self
            .prepare_call(chat_id, name, input, cancel)
            .await?
            .with_payload(context.clone()))
    }

    fn definitions(&self) -> Vec<ToolDefinition> {
        vec![ToolDefinition {
            name: "native_fixture".into(),
            description: "Native tool".into(),
            input_schema: json!({"type":"object","properties":{},"additionalProperties":false}),
        }]
    }

    fn workspace_host_context(&self) -> Option<WorkspaceHostContext> {
        Some(WorkspaceHostContext::from_environment().with_herdr_views())
    }

    fn permission(
        &self,
        _: tickloop_agent_protocol::ChatId,
        _: &str,
        _: &Value,
    ) -> HostToolPermission {
        HostToolPermission::ReadOnly
    }

    async fn execute(
        &self,
        _: tickloop_agent_protocol::ChatId,
        _: &str,
        _: Value,
        _: CancellationToken,
    ) -> ToolResult {
        ToolResult {
            content: "native".into(),
            is_error: false,
            output_ref: None,
        }
    }
}

#[tokio::test]
async fn native_tools_and_workspace_metadata_survive_mcp_composition() {
    let service = ArtifactService::start().await;
    let directory = tempfile::tempdir().unwrap();
    let provider = McpHostToolProvider::new(
        service.client(directory.path()),
        Some(Arc::new(NativeFixture)),
    );
    provider.refresh(CancellationToken::new()).await.unwrap();
    assert_eq!(
        provider
            .definitions()
            .iter()
            .map(|tool| tool.name.as_str())
            .collect::<Vec<_>>(),
        ["native_fixture", "ArtifactLogin"]
    );
    let context = provider.workspace_host_context().unwrap();
    assert_eq!(context.name, WorkspaceHostName::Herdr);
    assert_eq!(context.integration, WorkspaceIntegration::Available);
    assert_eq!(
        provider.host_endpoint().unwrap(),
        NativeFixture.host_endpoint().unwrap()
    );
    let context = HostCallContext {
        workdir: directory.path().into(),
        workspace_id: Some("work-context".into()),
        interaction: None,
    };
    let prepared = provider
        .prepare_call_in_context(
            uuid::Uuid::new_v4(),
            &context,
            "native_fixture",
            &json!({}),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    let captured = prepared
        .payload::<HostCallContext>()
        .expect("Native preparation receives its context");
    assert_eq!(captured.workdir, context.workdir);
    assert_eq!(captured.workspace_id, context.workspace_id);
}

#[tokio::test]
async fn remote_unauthorized_hides_tools_and_invalidates_local_login() {
    let service = ArtifactService::start().await;
    let directory = tempfile::tempdir().unwrap();
    let account = service.login(directory.path()).await;
    let provider = McpHostToolProvider::new(account, None);
    provider.refresh(CancellationToken::new()).await.unwrap();
    assert!(!provider.definitions().is_empty());
    service.control.reject_mcp.store(true, Ordering::SeqCst);
    provider.refresh(CancellationToken::new()).await.unwrap();
    assert_eq!(
        provider
            .definitions()
            .iter()
            .map(|tool| tool.name.as_str())
            .collect::<Vec<_>>(),
        ["ArtifactLogin"]
    );
    assert!(
        service
            .client(directory.path())
            .status()
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn stale_session_401_never_reports_a_replaced_login_as_signed_out() {
    let service = ArtifactService::start().await;
    let directory = tempfile::tempdir().unwrap();
    let account = service.login(directory.path()).await;
    let provider = Arc::new(McpHostToolProvider::new(account, None));
    service.control.signed_out.store(true, Ordering::SeqCst);
    service
        .control
        .hold_session_response
        .store(true, Ordering::SeqCst);
    let running = provider.clone();
    let refresh =
        tokio::spawn(async move { running.refresh(CancellationToken::new()).await.unwrap() });
    tokio::time::timeout(WAIT, service.control.session_response_started.notified())
        .await
        .unwrap();
    service
        .control
        .credential_revision
        .store(1, Ordering::SeqCst);
    let account = service.login(directory.path()).await;
    service.control.release_session_response.notify_one();
    refresh.await.unwrap();
    assert!(
        account.status().await.unwrap().is_some(),
        "the new login is valid and retained"
    );
    assert_ne!(
        provider.service_connections()["artifacts"]["authentication"],
        "signed_out",
        "a stale 401 response labelled the valid newer login signed_out"
    );
}

#[tokio::test]
async fn account_status_cache_requires_the_current_credential_generation() {
    let service = ArtifactService::start().await;
    let directory = tempfile::tempdir().unwrap();
    let provider = McpHostToolProvider::new(service.client(directory.path()), None);
    provider.refresh(CancellationToken::new()).await.unwrap();
    assert_eq!(
        provider.service_connections()["artifacts"]["authentication"],
        "signed_out"
    );
    service.login(directory.path()).await;
    assert_eq!(
        provider.service_connections()["artifacts"]["authentication"],
        "unknown"
    );
    provider.refresh(CancellationToken::new()).await.unwrap();
    assert_eq!(
        provider.service_connections()["artifacts"]["authentication"],
        "signed_in"
    );
    service
        .control
        .credential_revision
        .store(1, Ordering::SeqCst);
    service.login(directory.path()).await;
    assert_eq!(
        provider.service_connections()["artifacts"]["authentication"],
        "unknown"
    );
    provider.refresh(CancellationToken::new()).await.unwrap();
    assert_eq!(
        provider.service_connections()["artifacts"]["authentication"],
        "signed_in"
    );
}

#[tokio::test]
async fn linking_puts_the_account_link_and_claims_a_pending_request_into_its_loop() {
    let service = ArtifactService::start().await;
    let directory = tempfile::tempdir().unwrap();
    service.request_feedback("doc", "request-1");
    let model = Arc::new(ScriptedModel::new(vec![
        call(
            "reply",
            "ArtifactComment",
            json!({"action":"reply","artifact_id":"doc","thread_id":"thread-1","text":"Tightened."}),
        ),
        call(
            "done",
            "ArtifactComment",
            json!({"action":"done","artifact_id":"doc","outcome":"done","summary":"Tightened the introduction."}),
        ),
        text("Handled."),
    ]));
    let (agent, chat, loop_id) = linked_chat(&service, directory.path(), model.clone()).await;
    let link = service.link("doc").expect("the link was put");
    let put = &service.lifecycle_requests("PUT /artifact/api/v1/agent/links/doc")[0];
    assert_eq!(put["body"]["chat_id"], chat.to_string());
    assert_eq!(put["body"]["loop_id"], loop_id.to_string());
    assert_eq!(link.epoch, 1);
    eventually(async || service.finished() == [("request-1".into(), "done".into())]).await;
    wait_for_loop_state(&agent, chat, loop_id, LoopState::WaitingTrigger).await;

    let calls = service.tool_calls();
    assert_eq!(calls.len(), 2);
    for call in &calls {
        let context = &call["_meta"]["tickloop.ai/event-bridge"];
        assert_eq!(context["version"], 1);
        assert_eq!(context["source_key"], "doc");
        assert_eq!(context["binding_id"], link.binding_id);
        assert_eq!(context["association_epoch"], 1);
        assert_eq!(context["event_id"], "request-1");
        assert_eq!(context["claim_id"], "claim-request-1");
        assert_eq!(context["fencing_token"], "fence-request-1");
    }
    let conversations = model
        .requests
        .lock()
        .unwrap()
        .iter()
        .map(|request| serde_json::to_string(&request.messages).unwrap())
        .collect::<Vec<_>>();
    assert!(conversations[0].contains("@tickloop request request-1 on artifact doc"));
    assert!(conversations[0].contains("tighten the introduction"));
    assert!(
        conversations
            .iter()
            .all(|conversation| !conversation.contains("fence-request-1"))
    );
    let snapshot = agent.snapshot(chat).await.unwrap();
    let binding = &snapshot.loops[0].event_sources[0];
    assert_eq!(binding.artifact_id, "doc");
    assert_eq!(binding.account_id, "user-one");
    assert!(!binding.resume_required);
    assert_eq!(
        binding.last_request_at_ms,
        Some(artifact_service::REQUEST_CREATED_AT_MS)
    );
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(
        service.releases().is_empty(),
        "a done request is never released"
    );
    agent.shutdown().await;
}

/// Feedback requests are the artifact's Tick Updates and are never merged: each one the
/// association claims runs in the Loop on its own, in order.
#[tokio::test]
async fn feedback_requests_reach_the_loop_one_by_one() {
    let service = ArtifactService::start().await;
    let directory = tempfile::tempdir().unwrap();
    service.request_feedback("doc", "request-1");
    service.request_feedback("doc", "request-2");
    let done = |id| {
        call(
            id,
            "ArtifactComment",
            json!({"action":"done","artifact_id":"doc","outcome":"done","summary":"Handled."}),
        )
    };
    let model = Arc::new(ScriptedModel::new(vec![
        done("done-1"),
        text("Handled the first."),
        done("done-2"),
        text("Handled the second."),
    ]));
    let (agent, chat, loop_id) = linked_chat(&service, directory.path(), model.clone()).await;
    eventually(async || service.finished().len() == 2).await;
    wait_for_loop_state(&agent, chat, loop_id, LoopState::WaitingTrigger).await;
    assert_eq!(
        service.finished(),
        [
            ("request-1".into(), "done".into()),
            ("request-2".into(), "done".into())
        ]
    );
    assert_eq!(
        service
            .tool_calls()
            .iter()
            .map(|call| call["_meta"]["tickloop.ai/event-bridge"]["event_id"].clone())
            .collect::<Vec<_>>(),
        ["request-1", "request-2"],
        "each done answers the request its own run claimed"
    );
    let first_run = serde_json::to_string(&model.requests.lock().unwrap()[0].messages).unwrap();
    assert!(first_run.contains("@tickloop request request-1"));
    assert!(
        !first_run.contains("request-2"),
        "the second request waits for its own run"
    );
    agent.shutdown().await;
}

#[tokio::test]
async fn a_run_that_ends_without_done_fails_its_request() {
    let service = ArtifactService::start().await;
    let directory = tempfile::tempdir().unwrap();
    service.request_feedback("doc", "request-1");
    let model = Arc::new(ScriptedModel::new(vec![text("I looked at it.")]));
    let (agent, chat, loop_id) = linked_chat(&service, directory.path(), model).await;
    eventually(async || service.releases() == [("request-1".into(), "failed".into())]).await;
    wait_for_loop_state(&agent, chat, loop_id, LoopState::WaitingTrigger).await;
    assert!(!service.is_pending("request-1"));
    agent.shutdown().await;
}

struct BlockingModel {
    started: tokio::sync::Notify,
    turns: AtomicUsize,
}

#[async_trait]
impl ModelClient for BlockingModel {
    async fn generate_title(
        &self,
        request: ModelRequest,
        _: tokio::sync::mpsc::UnboundedSender<ModelDelta>,
        _: CancellationToken,
    ) -> Result<ModelOutput, ModelError> {
        Ok(tickloop_agent_test_support::title_output(request))
    }

    fn identity(&self) -> ModelIdentity {
        ScriptedModel::new(vec![]).identity()
    }

    async fn complete(
        &self,
        _: ModelRequest,
        _: tokio::sync::mpsc::UnboundedSender<ModelDelta>,
        cancel: CancellationToken,
    ) -> Result<ModelOutput, ModelError> {
        if self.turns.fetch_add(1, Ordering::SeqCst) == 0 {
            self.started.notify_one();
            cancel.cancelled().await;
            return Err(ModelError::Cancelled);
        }
        Ok(ModelOutput {
            private_source: None,
            content: text("Second attempt."),
            usage: Default::default(),
            stop_reason: "end_turn".into(),
        })
    }
}

#[tokio::test]
async fn an_interrupted_run_returns_its_request_and_continue_claims_it_again() {
    let service = ArtifactService::start().await;
    let directory = tempfile::tempdir().unwrap();
    service.request_feedback("doc", "request-1");
    let model = Arc::new(BlockingModel {
        started: tokio::sync::Notify::new(),
        turns: AtomicUsize::new(0),
    });
    let (agent, chat, loop_id) = linked_chat(&service, directory.path(), model.clone()).await;
    tokio::time::timeout(WAIT, model.started.notified())
        .await
        .unwrap();
    agent.interrupt(chat).await.unwrap();
    eventually(async || service.releases() == [("request-1".into(), "retry".into())]).await;
    wait_for_loop_state(&agent, chat, loop_id, LoopState::Interrupted).await;
    assert!(service.is_pending("request-1"));
    assert!(
        agent.snapshot(chat).await.unwrap().loops[0].event_sources[0].resume_required,
        "the interrupted Loop shows its feedback as paused"
    );
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert_eq!(
        model.turns.load(Ordering::SeqCst),
        1,
        "an interrupted Loop does not listen until Continue"
    );
    agent
        .link_artifact(chat, &artifact_tick(&service, "doc"))
        .await
        .unwrap();
    let snapshot = agent.snapshot(chat).await.unwrap();
    assert_eq!(
        snapshot.loops.len(),
        1,
        "Continue keeps the link in its Loop"
    );
    assert!(!snapshot.loops[0].event_sources[0].resume_required);
    eventually(async || model.turns.load(Ordering::SeqCst) == 2).await;
    eventually(async || service.releases().last() == Some(&("request-1".into(), "failed".into())))
        .await;
    agent.shutdown().await;
}

#[tokio::test]
async fn stopping_the_loop_hands_its_claimed_request_back_once() {
    let service = ArtifactService::start().await;
    let directory = tempfile::tempdir().unwrap();
    service.request_feedback("doc", "request-1");
    let model = Arc::new(BlockingModel {
        started: tokio::sync::Notify::new(),
        turns: AtomicUsize::new(0),
    });
    let (agent, chat, loop_id) = linked_chat(&service, directory.path(), model.clone()).await;
    tokio::time::timeout(WAIT, model.started.notified())
        .await
        .unwrap();
    agent.stop(chat, loop_id).await.unwrap();
    eventually(async || !service.releases().is_empty()).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(service.releases(), [("request-1".into(), "retry".into())]);
    assert!(service.is_pending("request-1"));
    agent.shutdown().await;
}

#[tokio::test]
async fn restart_waits_for_explicit_continue_which_links_again_and_claims_pending_requests() {
    let service = ArtifactService::start().await;
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("state");
    service.login(directory.path()).await;
    let agent = Agent::open_embedded(
        root.clone(),
        config(),
        Arc::new(ScriptedModel::new(vec![text("Ready.")])),
    )
    .unwrap();
    agent
        .install_host_tools(Arc::new(McpHostToolProvider::new(
            service.client(directory.path()),
            None,
        )))
        .unwrap();
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    agent
        .set_execution_mode(chat, ExecutionMode::FullAccess)
        .await
        .unwrap();
    agent.submit(chat, None, "Get ready".into()).await.unwrap();
    wait_for_completion(&agent, chat).await;
    agent
        .link_artifact(chat, &artifact_tick(&service, "doc"))
        .await
        .unwrap();
    let first_binding = service.link("doc").unwrap().binding_id;
    agent.shutdown().await;
    drop(agent);

    service.request_feedback("doc", "request-1");
    let model = Arc::new(ScriptedModel::new(vec![
        call(
            "done",
            "ArtifactComment",
            json!({"action":"done","artifact_id":"doc","outcome":"done"}),
        ),
        text("Handled after restart."),
    ]));
    let restored = Agent::open_embedded(root, config(), model.clone()).unwrap();
    restored
        .install_host_tools(Arc::new(McpHostToolProvider::new(
            service.client(directory.path()),
            None,
        )))
        .unwrap();
    let snapshot = restored.resume_chat(chat).await.unwrap();
    restored
        .set_execution_mode(chat, ExecutionMode::FullAccess)
        .await
        .unwrap();
    let goal = &snapshot.loops[0];
    assert_eq!(goal.state, LoopState::Interrupted);
    assert!(goal.event_sources[0].resume_required);
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert!(
        service.is_pending("request-1"),
        "history loading never listens"
    );
    restored
        .link_artifact(chat, &artifact_tick(&service, "doc"))
        .await
        .unwrap();
    let continued = restored.snapshot(chat).await.unwrap();
    assert_eq!(continued.loops.len(), 1);
    assert_eq!(continued.loops[0].id, goal.id);
    let renewed = service.link("doc").unwrap();
    assert_ne!(renewed.binding_id, first_binding);
    assert_eq!(renewed.epoch, 2);
    eventually(async || service.finished() == [("request-1".into(), "done".into())]).await;
    let binding = &restored.snapshot(chat).await.unwrap().loops[0].event_sources[0];
    assert_eq!(binding.binding_id.to_string(), renewed.binding_id);
    assert_eq!(binding.epoch, 2);
    restored.shutdown().await;
}

#[tokio::test]
async fn a_link_replaced_elsewhere_pauses_listening_until_continue() {
    let service = ArtifactService::start().await;
    let directory = tempfile::tempdir().unwrap();
    let (agent, chat, loop_id) = linked_chat(
        &service,
        directory.path(),
        Arc::new(ScriptedModel::new(vec![])),
    )
    .await;
    wait_for_loop_state(&agent, chat, loop_id, LoopState::WaitingTrigger).await;
    service.replace_link("doc");
    // The idle watcher checks again within its 15 second interval.
    tokio::time::timeout(Duration::from_secs(25), async {
        loop {
            let snapshot = agent.snapshot(chat).await.unwrap();
            if snapshot.loops[0].event_sources[0].resume_required {
                break snapshot;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .unwrap();
    wait_for_loop_state(&agent, chat, loop_id, LoopState::Interrupted).await;
    agent.shutdown().await;
}

#[tokio::test]
async fn signing_out_pauses_feedback_listening() {
    let service = ArtifactService::start().await;
    let directory = tempfile::tempdir().unwrap();
    let (agent, chat, loop_id) = linked_chat(
        &service,
        directory.path(),
        Arc::new(ScriptedModel::new(vec![])),
    )
    .await;
    wait_for_loop_state(&agent, chat, loop_id, LoopState::WaitingTrigger).await;
    agent.disable_event_sources().await;
    let snapshot = agent.snapshot(chat).await.unwrap();
    assert!(snapshot.loops[0].event_sources[0].resume_required);
    assert_eq!(snapshot.loops[0].state, LoopState::Interrupted);
    agent.shutdown().await;
}

#[tokio::test]
async fn relinking_an_artifact_keeps_it_in_the_loop_that_holds_it() {
    let service = ArtifactService::start().await;
    let directory = tempfile::tempdir().unwrap();
    let (agent, chat, loop_id) = linked_chat(
        &service,
        directory.path(),
        Arc::new(ScriptedModel::new(vec![text("Second goal.")])),
    )
    .await;
    agent
        .submit(chat, None, "An unrelated goal".into())
        .await
        .unwrap();
    wait_for_completion(&agent, chat).await;
    agent
        .link_artifact(chat, &artifact_tick(&service, "doc"))
        .await
        .unwrap();
    let snapshot = agent.snapshot(chat).await.unwrap();
    assert_eq!(snapshot.loops.len(), 2);
    assert_eq!(snapshot.loops[0].id, loop_id);
    assert_eq!(snapshot.loops[0].event_sources.len(), 1);
    assert_eq!(
        snapshot.loops[0].event_sources[0].binding_id.to_string(),
        service.link("doc").unwrap().binding_id
    );
    assert!(snapshot.loops[1].event_sources.is_empty());

    agent
        .link_artifact(chat, &artifact_tick(&service, "other"))
        .await
        .unwrap();
    let snapshot = agent.snapshot(chat).await.unwrap();
    assert_eq!(
        snapshot.loops[1].event_sources[0].artifact_id, "other",
        "a new link goes to the newest open Loop"
    );
    assert_eq!(snapshot.loops[1].state, LoopState::WaitingTrigger);
    agent.shutdown().await;
}

/// Holds its first turn until released, then answers every turn at once.
#[derive(Default)]
struct GatedModel {
    started: tokio::sync::Notify,
    release: tokio::sync::Notify,
    requests: std::sync::Mutex<Vec<ModelRequest>>,
}

#[async_trait]
impl ModelClient for GatedModel {
    async fn generate_title(
        &self,
        request: ModelRequest,
        _: tokio::sync::mpsc::UnboundedSender<ModelDelta>,
        _: CancellationToken,
    ) -> Result<ModelOutput, ModelError> {
        Ok(tickloop_agent_test_support::title_output(request))
    }

    fn identity(&self) -> ModelIdentity {
        ScriptedModel::new(vec![]).identity()
    }

    async fn complete(
        &self,
        request: ModelRequest,
        _: tokio::sync::mpsc::UnboundedSender<ModelDelta>,
        _: CancellationToken,
    ) -> Result<ModelOutput, ModelError> {
        let is_first = {
            let mut requests = self.requests.lock().unwrap();
            requests.push(request);
            requests.len() == 1
        };
        if is_first {
            self.started.notify_one();
            self.release.notified().await;
        }
        Ok(ModelOutput {
            private_source: None,
            content: text("Done."),
            usage: Default::default(),
            stop_reason: "end_turn".into(),
        })
    }
}

#[tokio::test]
async fn an_unlinked_artifact_no_longer_brings_its_requests_into_the_loop() {
    let service = ArtifactService::start().await;
    let directory = tempfile::tempdir().unwrap();
    let model = Arc::new(GatedModel::default());
    let agent = signed_in_agent(&service, directory.path(), model.clone()).await;
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    agent
        .set_execution_mode(chat, ExecutionMode::FullAccess)
        .await
        .unwrap();
    // The running Loop takes both artifacts and holds their requests back until it waits.
    agent
        .submit(chat, None, "Write two briefs".into())
        .await
        .unwrap();
    tokio::time::timeout(WAIT, model.started.notified())
        .await
        .unwrap();
    let loop_id = agent.snapshot(chat).await.unwrap().loops[0].id;
    service.request_feedback("doc", "request-1");
    service.request_feedback("other", "request-2");
    let doc = artifact_tick(&service, "doc");
    let other = artifact_tick(&service, "other");
    for tick in [&doc, &other] {
        agent.add_artifact_tick(chat, tick.clone()).await.unwrap();
        agent.link_artifact(chat, tick).await.unwrap();
    }
    agent
        .remove_artifact_tick(chat, &doc.instance_id())
        .await
        .unwrap();
    let snapshot = agent.snapshot(chat).await.unwrap();
    assert_eq!(snapshot.artifact_ticks, std::slice::from_ref(&other));
    assert_eq!(
        snapshot.loops[0]
            .event_sources
            .iter()
            .map(|source| source.artifact_id.as_str())
            .collect::<Vec<_>>(),
        ["other"]
    );

    model.release.notify_one();
    eventually(async || {
        service
            .releases()
            .contains(&("request-2".into(), "failed".into()))
    })
    .await;
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert!(service.is_pending("request-1"));
    assert!(
        service
            .lifecycle_requests("POST /artifact/api/v1/agent/requests/request-1/")
            .is_empty()
    );
    let conversations = model
        .requests
        .lock()
        .unwrap()
        .iter()
        .map(|request| serde_json::to_string(&request.messages).unwrap())
        .collect::<Vec<_>>();
    assert!(
        conversations
            .iter()
            .any(|conversation| conversation.contains("@tickloop request request-2"))
    );
    assert!(
        conversations
            .iter()
            .all(|conversation| !conversation.contains("request-1")),
        "an unlinked artifact's request never reaches the Loop"
    );

    wait_for_loop_state(&agent, chat, loop_id, LoopState::WaitingTrigger).await;
    agent
        .remove_artifact_tick(chat, &other.instance_id())
        .await
        .unwrap();
    let goal = agent.snapshot(chat).await.unwrap().loops[0].clone();
    assert!(goal.event_sources.is_empty());
    assert_eq!(
        goal.state,
        LoopState::Completed,
        "a Loop left with nothing to wait for completes"
    );
    agent.shutdown().await;
}

#[tokio::test]
async fn a_creation_answered_with_retired_wire_format_does_not_bind_monitor() {
    let service = ArtifactService::start().await;
    service
        .control
        .catalog_format_names
        .store(true, Ordering::SeqCst);
    let directory = tempfile::tempdir().unwrap();
    let model = Arc::new(ScriptedModel::new(vec![
        call(
            "create",
            "ArtifactEdit",
            json!({"action":"create","title":"Board","format":"page","content":"<h1>Board</h1>"}),
        ),
        text("Created."),
    ]));
    let agent = signed_in_agent(&service, directory.path(), model).await;
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    agent
        .submit(chat, None, "Mock up a board".into())
        .await
        .unwrap();
    wait_for_completion(&agent, chat).await;
    let snapshot = agent.snapshot(chat).await.unwrap();
    assert!(snapshot.artifact_ticks.is_empty());
    agent.shutdown().await;
}

#[tokio::test]
async fn a_read_artifact_joins_monitor_without_linking_to_feedback() {
    let service = ArtifactService::start().await;
    let directory = tempfile::tempdir().unwrap();
    let model = Arc::new(ScriptedModel::new(vec![
        call(
            "read",
            "ArtifactRead",
            json!({"action":"read","artifact_id":"doc"}),
        ),
        text("Read."),
    ]));
    let agent = signed_in_agent(&service, directory.path(), model).await;
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    submit_and_wait(&agent, chat, "Summarize this document").await;
    let snapshot = agent.snapshot(chat).await.unwrap();
    assert_eq!(snapshot.artifact_ticks.len(), 1);
    let tick = &snapshot.artifact_ticks[0];
    assert_eq!(tick.artifact_id, "doc");
    assert_eq!(tick.origin, service.origin);
    assert_eq!(tick.title, "Fixture");
    assert_eq!(tick.format, "markdown");
    assert_eq!(tick.access_role, "owner");
    assert!(
        snapshot
            .loops
            .iter()
            .all(|goal| goal.event_sources.is_empty())
    );
    assert!(service.link("doc").is_none());
    assert!(
        service
            .lifecycle_requests("PUT /artifact/api/v1/agent/links/")
            .is_empty()
    );
    assert_eq!(service.tool_calls().len(), 1);
    agent.shutdown().await;
}

#[tokio::test]
async fn a_read_artifact_has_one_canonical_monitor_row_across_pages_and_restore() {
    let service = ArtifactService::start().await;
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("state");
    let canonical_id = "e1b85a39-a57b-4c28-a8a5-ae9233aead3e";
    let short_id = "6rvWPvOnhVABug7llbHb26";
    let body = json!({"artifact_id":canonical_id,"title":"Fixture","format":"doc","url":tickloop_agent_artifacts::artifact_url(&service.origin, canonical_id),"access_role":"owner","revision":3,"blocks":[{"id":"b1","hash":"h1","text":"# Fixture"}],"next_cursor":"1"});
    *service.control.read_result.lock().unwrap() = Some(
        json!({"content":[{"type":"text","text":body.to_string()}],"structuredContent":body,"isError":false}),
    );
    let account = service.login(directory.path()).await;
    let model = Arc::new(ScriptedModel::new(vec![
        call(
            "short",
            "ArtifactRead",
            json!({"action":"read","artifact_id":short_id}),
        ),
        text("First page."),
        call(
            "next",
            "ArtifactRead",
            json!({"action":"read","artifact_id":canonical_id,"cursor":"1"}),
        ),
        call(
            "repeat",
            "ArtifactRead",
            json!({"action":"read","artifact_id":short_id}),
        ),
        text("Read."),
        call(
            "removed",
            "ArtifactRead",
            json!({"action":"read","artifact_id":short_id}),
        ),
        text("Still removed."),
    ]));
    let agent = Agent::open_embedded(root.clone(), config(), model.clone()).unwrap();
    agent
        .install_host_tools(Arc::new(McpHostToolProvider::new(account, None)))
        .unwrap();
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    submit_and_wait(&agent, chat, "Read this document").await;
    let first = agent.snapshot(chat).await.unwrap();
    assert_eq!(first.artifact_ticks.len(), 1);
    assert_eq!(first.artifact_ticks[0].artifact_id, canonical_id);
    submit_and_wait(&agent, chat, "Read the next page and reread the document").await;
    let snapshot = agent.snapshot(chat).await.unwrap();
    assert_eq!(snapshot.artifact_ticks, first.artifact_ticks);
    assert!(
        snapshot
            .loops
            .iter()
            .all(|goal| goal.event_sources.is_empty())
    );
    assert!(
        snapshot
            .monitor
            .taken_in_at(&snapshot.artifact_ticks[0].instance_id().parse().unwrap())
            .is_some()
    );
    agent.shutdown().await;

    let restored = Agent::open_embedded(root, config(), model).unwrap();
    restored
        .install_host_tools(Arc::new(McpHostToolProvider::new(
            service.client(directory.path()),
            None,
        )))
        .unwrap();
    let resumed = restored.resume_chat(chat).await.unwrap();
    assert_eq!(resumed.artifact_ticks, snapshot.artifact_ticks);
    restored
        .remove_artifact_tick(chat, &resumed.artifact_ticks[0].instance_id())
        .await
        .unwrap();
    submit_and_wait(
        &restored,
        chat,
        "Read it again without restoring it to Monitor",
    )
    .await;
    assert!(
        restored
            .snapshot(chat)
            .await
            .unwrap()
            .artifact_ticks
            .is_empty()
    );
    assert!(
        service
            .lifecycle_requests("PUT /artifact/api/v1/agent/links/")
            .is_empty()
    );
    assert!(
        service
            .tool_calls()
            .iter()
            .all(|call| call["name"] == "ArtifactRead")
    );
    restored.shutdown().await;
}

#[tokio::test]
async fn a_read_artifact_preserves_response_roles_and_supported_formats() {
    let service = ArtifactService::start().await;
    let directory = tempfile::tempdir().unwrap();
    let model = Arc::new(ScriptedModel::new(vec![]));
    let agent = signed_in_agent(&service, directory.path(), model.clone()).await;
    for (format, role, stored_format, stored_role) in [
        ("page", "edit", "html", "editor"),
        ("page", "view", "html", "viewer"),
        ("doc", "view", "markdown", "viewer"),
    ] {
        *service.control.read_result.lock().unwrap() =
            Some(artifact_body_result(&service, format, role));
        model.enqueue(call(
            "read",
            "ArtifactRead",
            json!({"action":"read","artifact_id":"doc"}),
        ));
        model.enqueue(text("Read."));
        let chat = agent.create_chat(directory.path().into()).await.unwrap();
        submit_and_wait(&agent, chat, "Read this artifact").await;
        let snapshot = agent.snapshot(chat).await.unwrap();
        assert_eq!(snapshot.artifact_ticks.len(), 1);
        assert_eq!(snapshot.artifact_ticks[0].format, stored_format);
        assert_eq!(snapshot.artifact_ticks[0].access_role, stored_role);
        assert!(
            snapshot
                .loops
                .iter()
                .all(|goal| goal.event_sources.is_empty())
        );
    }
    assert!(
        service
            .lifecycle_requests("PUT /artifact/api/v1/agent/links/")
            .is_empty()
    );
    agent.shutdown().await;
}

fn artifact_body_result(service: &ArtifactService, format: &str, role: &str) -> Value {
    let mut body = json!({"artifact_id":"doc","title":"Fixture","format":format,"url":tickloop_agent_artifacts::artifact_url(&service.origin, "doc"),"access_role":role,"revision":3});
    if format == "doc" {
        body["blocks"] = json!([{"id":"b1","hash":"h1","text":"# Fixture"}]);
        body["next_cursor"] = Value::Null;
    } else {
        body["source"] = json!("<h1>Fixture</h1>");
    }
    json!({"content":[{"type":"text","text":body.to_string()}],"structuredContent":body,"isError":false})
}

#[tokio::test]
async fn artifact_reads_attach_only_valid_matching_body_receipts() {
    let service = ArtifactService::start().await;
    let directory = tempfile::tempdir().unwrap();
    let model = Arc::new(ScriptedModel::new(vec![]));
    let agent = signed_in_agent(&service, directory.path(), model.clone()).await;
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    for (field, value) in [
        ("artifact_id", json!("another-artifact")),
        ("url", json!("https://another.example/artifact/doc")),
        (
            "url",
            json!(format!("{}/artifact/another-artifact", service.origin)),
        ),
        ("format", json!("markdown")),
        ("access_role", json!("admin")),
        ("title", json!("")),
        ("revision", Value::Null),
        ("blocks", Value::Null),
        ("isError", json!(true)),
    ] {
        let mut result = artifact_body_result(&service, "doc", "owner");
        if field == "isError" {
            result[field] = value;
        } else {
            result["structuredContent"][field] = value;
        }
        *service.control.read_result.lock().unwrap() = Some(result);
        model.enqueue(call(
            "read",
            "ArtifactRead",
            json!({"action":"read","artifact_id":"doc"}),
        ));
        model.enqueue(text("Read checked."));
        submit_and_wait(&agent, chat, "Read this document").await;
        assert!(
            agent
                .snapshot(chat)
                .await
                .unwrap()
                .artifact_ticks
                .is_empty(),
            "invalid {field} receipt attached an artifact"
        );
    }
    *service.control.read_result.lock().unwrap() =
        Some(artifact_body_result(&service, "doc", "owner"));
    model.enqueue(call("list", "ArtifactRead", json!({"action":"list"})));
    model.enqueue(call(
        "threads",
        "ArtifactRead",
        json!({"action":"threads","artifact_id":"doc"}),
    ));
    model.enqueue(text("Listed."));
    submit_and_wait(&agent, chat, "List artifacts and read comments").await;
    let snapshot = agent.snapshot(chat).await.unwrap();
    assert!(snapshot.artifact_ticks.is_empty());
    assert!(
        snapshot
            .loops
            .iter()
            .all(|goal| goal.event_sources.is_empty())
    );
    assert!(
        service
            .lifecycle_requests("PUT /artifact/api/v1/agent/links/")
            .is_empty()
    );
    assert!(
        service
            .tool_calls()
            .iter()
            .all(|call| call["name"] == "ArtifactRead")
    );
    agent.shutdown().await;
}

#[tokio::test]
async fn a_created_artifact_joins_monitor_and_links_to_the_creating_loop() {
    let service = ArtifactService::start().await;
    let directory = tempfile::tempdir().unwrap();
    let model = Arc::new(ScriptedModel::new(vec![
        call(
            "create",
            "ArtifactEdit",
            json!({"action":"create","title":"Brief","format":"doc","content":"# Brief"}),
        ),
        text("Created."),
    ]));
    let agent = signed_in_agent(&service, directory.path(), model.clone()).await;
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    let mut events = agent.subscribe();
    agent
        .submit(chat, None, "Write a brief".into())
        .await
        .unwrap();
    eventually(async || {
        agent
            .snapshot(chat)
            .await
            .unwrap()
            .loops
            .last()
            .is_some_and(|goal| goal.state == LoopState::WaitingTrigger)
    })
    .await;
    while let Ok(event) = events.try_recv() {
        assert!(
            !matches!(
                event.kind,
                EventKind::PermissionRequested { .. } | EventKind::ApprovalAssessed { .. }
            ),
            "ordinary first-party creation must not ask or invoke a reviewer"
        );
    }
    assert!(
        agent.approval_history_view(chat).await.unwrap().records[0]
            .review
            .is_none()
    );
    let snapshot = agent.snapshot(chat).await.unwrap();
    assert_eq!(snapshot.artifact_ticks.len(), 1);
    assert_eq!(snapshot.artifact_ticks[0].artifact_id, CREATED_ARTIFACT);
    assert_eq!(snapshot.artifact_ticks[0].title, "Brief");
    assert_eq!(
        snapshot.artifact_ticks[0].format, "markdown",
        "a doc is catalogued under its stored format"
    );
    let goal = snapshot.loops.last().unwrap();
    assert_eq!(goal.event_sources[0].artifact_id, CREATED_ARTIFACT);
    let link = service.link(CREATED_ARTIFACT).unwrap();
    assert_eq!(link.loop_id, goal.id.to_string());
    assert_eq!(link.chat_id, chat.to_string());
    assert!(tick_protocol::InstanceRef::artifact(&service.origin, CREATED_ARTIFACT).is_ok());
    agent.shutdown().await;
}

#[tokio::test]
async fn a_failed_creation_link_is_reported_to_the_model_and_monitor_still_shows_it() {
    let service = ArtifactService::start().await;
    let directory = tempfile::tempdir().unwrap();
    service.control.fail_link.store(true, Ordering::SeqCst);
    let model = Arc::new(ScriptedModel::new(vec![
        call(
            "create",
            "ArtifactEdit",
            json!({"action":"create","title":"Brief","format":"page","content":"<p>Brief</p>"}),
        ),
        text("Created."),
    ]));
    let agent = signed_in_agent(&service, directory.path(), model.clone()).await;
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    agent
        .set_execution_mode(chat, ExecutionMode::FullAccess)
        .await
        .unwrap();
    agent
        .submit(chat, None, "Write a brief".into())
        .await
        .unwrap();
    wait_for_completion(&agent, chat).await;
    let snapshot = agent.snapshot(chat).await.unwrap();
    assert_eq!(snapshot.artifact_ticks.len(), 1);
    assert_eq!(snapshot.artifact_ticks[0].format, "html");
    assert!(snapshot.loops[0].event_sources.is_empty());
    let result = serde_json::to_string(&model.requests.lock().unwrap()[1].messages).unwrap();
    assert!(
        result.contains("linking it for @tickloop requests failed"),
        "{result}"
    );
    agent.shutdown().await;
}

#[tokio::test]
async fn continuing_after_an_account_switch_links_the_artifact_for_the_new_account() {
    let service = ArtifactService::start().await;
    let directory = tempfile::tempdir().unwrap();
    let (agent, chat, loop_id) = linked_chat(
        &service,
        directory.path(),
        Arc::new(ScriptedModel::new(vec![])),
    )
    .await;
    wait_for_loop_state(&agent, chat, loop_id, LoopState::WaitingTrigger).await;
    service.control.other_account.store(true, Ordering::SeqCst);
    service.login(directory.path()).await;
    tokio::time::timeout(Duration::from_secs(25), async {
        while !agent.snapshot(chat).await.unwrap().loops[0].event_sources[0].resume_required {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("the previous account's link pauses");
    agent
        .link_artifact(chat, &artifact_tick(&service, "doc"))
        .await
        .unwrap();
    let snapshot = agent.snapshot(chat).await.unwrap();
    assert_eq!(snapshot.loops.len(), 1);
    let sources = &snapshot.loops[0].event_sources;
    assert_eq!(
        sources.len(),
        1,
        "the new account's link replaces the old one"
    );
    assert_eq!(sources[0].account_id, "user-two");
    assert!(!sources[0].resume_required);
    assert_eq!(snapshot.loops[0].state, LoopState::WaitingTrigger);
    agent.shutdown().await;
}

const HOST_NOTICE: &str = "Host notice:";

fn host_notices(request: &ModelRequest) -> Vec<String> {
    request
        .messages
        .iter()
        .flat_map(|message| &message.content)
        .filter_map(|block| match block {
            Content::Text { text } if text.starts_with(HOST_NOTICE) => Some(text.clone()),
            _ => None,
        })
        .collect()
}

fn tool_result_text(request: &ModelRequest, call_id: &str) -> String {
    request
        .messages
        .iter()
        .flat_map(|message| &message.content)
        .find_map(|block| match block {
            Content::ToolResult {
                tool_use_id,
                content,
                ..
            } if tool_use_id == call_id => Some(content.text().into_owned()),
            _ => None,
        })
        .unwrap_or_else(|| panic!("no tool result for {call_id}"))
}

async fn submit_and_wait(agent: &Agent, chat: ChatId, prompt: &str) {
    agent.submit(chat, None, prompt.into()).await.unwrap();
    wait_for_completion(agent, chat).await;
}

#[tokio::test]
async fn a_resumed_chat_is_told_once_which_host_tools_became_available() {
    let service = ArtifactService::start().await;
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("root");
    let earlier = Agent::open_embedded(
        root.clone(),
        config(),
        Arc::new(ScriptedModel::new(vec![text(
            "I do not have ArtifactEdit in this conversation.",
        )])),
    )
    .unwrap();
    let chat = earlier.create_chat(directory.path().into()).await.unwrap();
    submit_and_wait(&earlier, chat, "Make a doc").await;
    earlier.shutdown().await;

    let account = service.login(directory.path()).await;
    let model = Arc::new(ScriptedModel::new(vec![
        text("Now I can."),
        text("Still can."),
    ]));
    let resumed = Agent::open_embedded(root.clone(), config(), model.clone()).unwrap();
    resumed
        .install_host_tools(Arc::new(McpHostToolProvider::new(account, None)))
        .unwrap();
    resumed.resume_chat(chat).await.unwrap();
    submit_and_wait(&resumed, chat, "Try again").await;
    submit_and_wait(&resumed, chat, "And once more").await;
    let snapshot = resumed.snapshot(chat).await.unwrap();
    resumed.shutdown().await;
    {
        let requests = model.requests.lock().unwrap();
        let notices = host_notices(&requests[0]);
        assert_eq!(notices.len(), 1, "{notices:?}");
        assert!(
            notices[0].contains(
                "host tools available in this conversation now: ArtifactComment, ArtifactEdit, ArtifactRead."
            ),
            "{}",
            notices[0]
        );
        assert!(!notices[0].contains("changed"), "{}", notices[0]);
        assert!(notices[0].contains("may be outdated"));
        assert!(tool_names(&requests[0]).contains(&"ArtifactEdit"));
        assert_eq!(
            host_notices(&requests[1]).len(),
            1,
            "an unchanged tool set is not announced again"
        );
    }
    assert_eq!(snapshot.host_notes.len(), 1);
    let (index, announced) = snapshot.host_notes.iter().next().unwrap();
    assert!(
        announced
            .announced_host_tools()
            .is_some_and(|tools| tools.contains("ArtifactRead"))
    );
    assert!(
        !snapshot.prompt_execution_modes.contains_key(index),
        "a host note is never a user prompt"
    );
    let public = serde_json::to_string(&snapshot.public_conversation_items().unwrap()).unwrap();
    assert!(!public.contains(HOST_NOTICE), "{public}");

    let account = service.client(directory.path());
    let model = Arc::new(ScriptedModel::new(vec![text("Same tools.")]));
    let reopened = Agent::open_embedded(root, config(), model.clone()).unwrap();
    reopened
        .install_host_tools(Arc::new(McpHostToolProvider::new(account, None)))
        .unwrap();
    let restored = reopened.resume_chat(chat).await.unwrap();
    assert_eq!(restored.host_notes, snapshot.host_notes);
    submit_and_wait(&reopened, chat, "Continue").await;
    reopened.shutdown().await;
    assert_eq!(
        host_notices(&model.requests.lock().unwrap()[0]).len(),
        1,
        "the announced set survives resume"
    );
}

#[tokio::test]
async fn a_new_chat_is_not_told_about_tools_its_first_request_lists() {
    let service = ArtifactService::start().await;
    let directory = tempfile::tempdir().unwrap();
    let model = Arc::new(ScriptedModel::new(vec![text("Ready."), text("Again.")]));
    let agent = signed_in_agent(&service, directory.path(), model.clone()).await;
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    submit_and_wait(&agent, chat, "Hello").await;
    submit_and_wait(&agent, chat, "Hello again").await;
    agent.shutdown().await;
    let requests = model.requests.lock().unwrap();
    assert!(tool_names(&requests[0]).contains(&"ArtifactRead"));
    for request in requests.iter() {
        assert!(host_notices(request).is_empty());
    }
}

#[tokio::test]
async fn signing_out_announces_the_removed_tools_and_offers_sign_in() {
    let service = ArtifactService::start().await;
    let directory = tempfile::tempdir().unwrap();
    let model = Arc::new(ScriptedModel::new(vec![
        text("Ready."),
        text("Signed out."),
    ]));
    let agent = signed_in_agent(&service, directory.path(), model.clone()).await;
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    submit_and_wait(&agent, chat, "Hello").await;
    service.client(directory.path()).logout().await.unwrap();
    submit_and_wait(&agent, chat, "Make a design").await;
    agent.shutdown().await;
    let requests = model.requests.lock().unwrap();
    let notices = host_notices(&requests[1]);
    assert_eq!(notices.len(), 1);
    assert!(
        notices[0].contains("Now available: ArtifactLogin.")
            && notices[0]
                .contains("No longer available: ArtifactComment, ArtifactEdit, ArtifactRead."),
        "{}",
        notices[0]
    );
    let names = tool_names(&requests[1]);
    assert!(names.contains(&"ArtifactLogin"));
    assert!(!names.contains(&"ArtifactEdit"));
}

#[tokio::test]
async fn signed_out_offers_only_artifact_login() {
    let service = ArtifactService::start().await;
    let directory = tempfile::tempdir().unwrap();
    let provider = McpHostToolProvider::new(service.client(directory.path()), None);
    assert!(
        provider.definitions().is_empty(),
        "an unverified account offers nothing"
    );
    provider.refresh(CancellationToken::new()).await.unwrap();
    let definitions = provider.definitions();
    assert_eq!(
        definitions
            .iter()
            .map(|tool| tool.name.as_str())
            .collect::<Vec<_>>(),
        ["ArtifactLogin"]
    );
    assert_eq!(
        definitions[0].input_schema,
        json!({"type":"object","properties":{},"additionalProperties":false})
    );
    assert_eq!(
        provider.permission(ChatId::new_v4(), "ArtifactLogin", &json!({})),
        HostToolPermission::Automatic
    );
    assert!(
        provider
            .validate_input(
                "ArtifactLogin",
                &definitions[0].input_schema,
                &json!({"x":1})
            )
            .is_err()
    );
}

#[tokio::test]
async fn artifact_login_returns_the_link_and_code_and_approval_brings_the_tools() {
    let service = ArtifactService::start().await;
    let directory = tempfile::tempdir().unwrap();
    service
        .control
        .hold_device_approval
        .store(true, Ordering::SeqCst);
    let model = Arc::new(ScriptedModel::new(vec![
        call("login", "ArtifactLogin", json!({})),
        call("login-again", "ArtifactLogin", json!({})),
        text("Open the link and confirm the code."),
        text("Signed in."),
    ]));
    let agent = Agent::new_embedded(config(), model.clone());
    agent
        .install_host_tools(Arc::new(McpHostToolProvider::new(
            service.client(directory.path()),
            None,
        )))
        .unwrap();
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    submit_and_wait(&agent, chat, "Make me a design").await;
    {
        let requests = model.requests.lock().unwrap();
        let names = tool_names(&requests[0]);
        assert!(names.contains(&"ArtifactLogin"));
        assert!(!names.contains(&"ArtifactEdit"));
        let started = tool_result_text(&requests[1], "login");
        assert!(started.starts_with("Sign-in started."), "{started}");
        assert!(
            started.contains(&format!("{}/artifact/device?code=TEST", service.origin))
                && started.contains("confirm the code TEST"),
            "{started}"
        );
        let repeated = tool_result_text(&requests[2], "login-again");
        assert!(
            repeated.starts_with("Sign-in is already pending."),
            "{repeated}"
        );
    }
    assert_eq!(
        service.device_logins_started(),
        1,
        "sign-in is single-flight"
    );

    service
        .control
        .hold_device_approval
        .store(false, Ordering::SeqCst);
    let account = service.client(directory.path());
    eventually(async || matches!(account.credential_snapshot(), Ok(Some(_)))).await;
    submit_and_wait(&agent, chat, "Now make it").await;
    agent.shutdown().await;
    let requests = model.requests.lock().unwrap();
    let names = tool_names(&requests[3]);
    for name in ["ArtifactRead", "ArtifactEdit", "ArtifactComment"] {
        assert!(names.contains(&name), "{name}");
    }
    assert!(!names.contains(&"ArtifactLogin"));
    let notices = host_notices(&requests[3]);
    assert_eq!(notices.len(), 1);
    assert!(
        notices[0].contains("Now available: ArtifactComment, ArtifactEdit, ArtifactRead.")
            && notices[0].contains("No longer available: ArtifactLogin."),
        "{}",
        notices[0]
    );
}

#[tokio::test]
async fn a_sign_in_during_a_turn_reaches_the_next_request_of_that_turn() {
    let service = ArtifactService::start().await;
    let directory = tempfile::tempdir().unwrap();
    let model = Arc::new(ScriptedModel::new(vec![
        call(
            "ask",
            tickloop_agent_tools::ASK_USER_QUESTION_TOOL_NAME,
            json!({"questions":[{"question":"Signed in yet?","header":"Sign-in","options":[
                {"label":"Yes","description":"Signed in with /artifacts"},
                {"label":"No","description":"Not yet"}
            ]}]}),
        ),
        text("The Artifact tools are available now."),
    ]));
    let agent = Agent::new_embedded(config(), model.clone());
    agent
        .install_host_tools(Arc::new(McpHostToolProvider::new(
            service.client(directory.path()),
            None,
        )))
        .unwrap();
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    let mut events = agent.subscribe();
    agent
        .submit(chat, None, "Write a doc".into())
        .await
        .unwrap();
    let question = loop {
        if let EventKind::QuestionAsked { request } =
            tickloop_agent_test_support::next_event(&mut events)
                .await
                .kind
        {
            break request;
        }
    };
    service.login(directory.path()).await;
    agent
        .answer_question(
            chat,
            question.id,
            vec![tickloop_agent_protocol::QuestionAnswer::option("Yes")],
        )
        .await
        .unwrap();
    wait_for_completion(&agent, chat).await;
    agent.shutdown().await;
    let requests = model.requests.lock().unwrap();
    assert!(!tool_names(&requests[0]).contains(&"ArtifactEdit"));
    assert!(tool_names(&requests[1]).contains(&"ArtifactEdit"));
    assert_eq!(host_notices(&requests[1]).len(), 1);
}

#[tokio::test]
async fn a_child_delegated_before_sign_in_takes_up_the_artifact_tools() {
    let service = ArtifactService::start().await;
    let directory = tempfile::tempdir().unwrap();
    let model = Arc::new(ScriptedModel::new(vec![
        call(
            "delegate",
            "Subagent",
            json!({"task":"Prepare an Artifact draft"}),
        ),
        call(
            "wait",
            tickloop_agent_tools::ASK_USER_QUESTION_TOOL_NAME,
            json!({"questions":[{"question":"Signed in yet?","header":"Sign-in","options":[
                {"label":"Yes","description":"Signed in with /artifacts"},
                {"label":"No","description":"Not yet"}
            ]}]}),
        ),
        call(
            "read",
            "ArtifactRead",
            json!({"action":"read","artifact_id":"doc"}),
        ),
        text("Child read the doc."),
        text("Done."),
    ]));
    let agent = Agent::new_embedded(config(), model.clone());
    agent
        .install_host_tools(Arc::new(McpHostToolProvider::new(
            service.client(directory.path()),
            None,
        )))
        .unwrap();
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    let mut events = agent.subscribe();
    agent
        .submit(chat, None, "Delegate the draft".into())
        .await
        .unwrap();
    let (child, question) = loop {
        let event = tickloop_agent_test_support::next_event(&mut events).await;
        if let EventKind::QuestionAsked { request } = event.kind {
            break (event.chat_id, request);
        }
    };
    assert_ne!(child, chat, "the child asks the question");
    service.login(directory.path()).await;
    agent
        .answer_question(
            child,
            question.id,
            vec![tickloop_agent_protocol::QuestionAnswer::option("Yes")],
        )
        .await
        .unwrap();
    wait_for_completion(&agent, chat).await;
    let pinned = agent
        .snapshot(child)
        .await
        .unwrap()
        .delegation
        .unwrap()
        .ceiling
        .host_tools;
    agent.shutdown().await;
    let requests = model.requests.lock().unwrap();
    assert_eq!(requests.len(), 5);
    assert!(tool_names(&requests[0]).contains(&"ArtifactLogin"));
    let child_before = tool_names(&requests[1]);
    assert!(!child_before.contains(&"ArtifactEdit"));
    assert!(
        !child_before.contains(&"ArtifactLogin"),
        "a child never starts a sign-in"
    );
    assert!(tool_names(&requests[2]).contains(&"ArtifactEdit"));
    assert_eq!(host_notices(&requests[2]).len(), 1);
    for name in ["ArtifactRead", "ArtifactEdit", "ArtifactComment"] {
        assert!(
            pinned[name].iter().any(|authority| matches!(
                authority,
                tickloop_agent_protocol::HostToolAuthority::Mcp { account_id, .. } if account_id == "user-one"
            )),
            "{name} is pinned to the first account signed in"
        );
    }
    let read = tool_result_text(&requests[3], "read");
    assert!(read.contains("# Fixture body"), "{read}");
    assert_eq!(service.tool_calls().len(), 1);
}

#[tokio::test]
async fn a_passing_outage_writes_no_tool_notices() {
    let service = ArtifactService::start().await;
    let directory = tempfile::tempdir().unwrap();
    let model = Arc::new(ScriptedModel::new(vec![
        text("Ready."),
        text("Session down."),
        text("Directory down."),
        text("Back."),
    ]));
    let agent = signed_in_agent(&service, directory.path(), model.clone()).await;
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    submit_and_wait(&agent, chat, "Hello").await;
    service
        .control
        .session_unavailable
        .store(true, Ordering::SeqCst);
    submit_and_wait(&agent, chat, "Still there?").await;
    service
        .control
        .session_unavailable
        .store(false, Ordering::SeqCst);
    service
        .control
        .directory_unavailable
        .store(true, Ordering::SeqCst);
    submit_and_wait(&agent, chat, "And now?").await;
    service
        .control
        .directory_unavailable
        .store(false, Ordering::SeqCst);
    submit_and_wait(&agent, chat, "Back?").await;
    agent.shutdown().await;
    let requests = model.requests.lock().unwrap();
    assert!(!tool_names(&requests[1]).contains(&"ArtifactEdit"));
    assert!(!tool_names(&requests[2]).contains(&"ArtifactEdit"));
    assert!(tool_names(&requests[3]).contains(&"ArtifactEdit"));
    for request in requests.iter() {
        assert!(
            host_notices(request).is_empty(),
            "{:?}",
            host_notices(request)
        );
    }
}

#[tokio::test]
async fn a_request_after_compaction_still_carries_the_tool_notice() {
    let service = ArtifactService::start().await;
    let directory = tempfile::tempdir().unwrap();
    let model = Arc::new(ScriptedModel::new(vec![
        text(&"older material ".repeat(2000)),
        text("Compact summary"),
        text("The Artifact tools are here."),
    ]));
    let agent = Agent::new_embedded(config(), model.clone());
    agent
        .install_host_tools(Arc::new(McpHostToolProvider::new(
            service.client(directory.path()),
            None,
        )))
        .unwrap();
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    submit_and_wait(&agent, chat, "Collect the material").await;
    service.login(directory.path()).await;
    agent.set_context_window(chat, 20_000).await.unwrap();
    submit_and_wait(&agent, chat, "Now write the doc").await;
    agent.shutdown().await;
    let requests = model.requests.lock().unwrap();
    assert!(
        requests[1].tools.is_empty(),
        "the second request is the summary"
    );
    let notices = host_notices(&requests[2]);
    assert_eq!(notices.len(), 1, "{notices:?}");
    assert!(
        notices[0].contains(
            "host tools available in this conversation now: ArtifactComment, ArtifactEdit, ArtifactRead."
        ),
        "{}",
        notices[0]
    );
}

#[tokio::test]
async fn remote_skills_survive_compaction_but_changed_manifests_do_not() {
    for changed in [false, true] {
        let service = ArtifactService::start().await;
        service.control.skills_enabled.store(true, Ordering::SeqCst);
        let directory = tempfile::tempdir().unwrap();
        let model = Arc::new(ScriptedModel::new(vec![
            call(
                "load",
                "Skill",
                json!({"skill":"tickloop-artifacts:artifact-design","args":"keep these arguments"}),
            ),
            call(
                "reference",
                "Skill",
                json!({"skill":"tickloop-artifacts:artifact-design","resource":"references/runtime.md"}),
            ),
            text("done"),
            text("A compact summary"),
            text("continued"),
        ]));
        let agent = signed_in_agent(&service, directory.path(), model.clone()).await;
        let chat = agent.create_chat(directory.path().into()).await.unwrap();
        submit_and_wait(&agent, chat, "Build a page").await;
        agent.compact(chat).await.unwrap();
        if changed {
            service.control.skill_revision.store(1, Ordering::SeqCst);
        }
        submit_and_wait(&agent, chat, "Continue the page").await;
        {
            let requests = model.requests.lock().unwrap();
            let last = &requests.last().unwrap().messages;
            let guidance = last
                .iter()
                .flat_map(|message| &message.content)
                .find_map(|block| match block {
                    Content::Text { text }
                        if text.starts_with(
                            "Reference guidance from tickloop-artifacts:artifact-design",
                        ) =>
                    {
                        Some(text)
                    }
                    _ => None,
                });
            assert_eq!(guidance.is_some(), !changed);
            if let Some(guidance) = guidance {
                assert!(guidance.contains("keep these arguments"));
                assert!(guidance.contains("Reference fixture: preserve data units"));
            }
        }
        agent.shutdown().await;
    }
}

#[tokio::test]
async fn remote_skill_reference_requested_after_compaction_reloads_truncated_guidance() {
    let service = ArtifactService::start().await;
    service.control.skills_enabled.store(true, Ordering::SeqCst);
    service
        .control
        .skill_reference_bytes
        .store(50_000, Ordering::SeqCst);
    let directory = tempfile::tempdir().unwrap();
    let model = Arc::new(ScriptedModel::new(vec![
        call(
            "load",
            "Skill",
            json!({"skill":"tickloop-artifacts:artifact-design","args":"preserve these arguments"}),
        ),
        call(
            "reference",
            "Skill",
            json!({"skill":"tickloop-artifacts:artifact-design","resource":"references/runtime.md"}),
        ),
        text("done"),
        text("A compact summary"),
        call(
            "reload",
            "Skill",
            json!({"skill":"tickloop-artifacts:artifact-design","resource":"references/runtime.md"}),
        ),
        text("continued"),
    ]));
    let agent = signed_in_agent(&service, directory.path(), model.clone()).await;
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    submit_and_wait(&agent, chat, "Build a page").await;
    agent.compact(chat).await.unwrap();
    submit_and_wait(&agent, chat, "Use the complete runtime reference").await;
    {
        let requests = model.requests.lock().unwrap();
        let guidance = |index: usize| {
            requests[index]
                .messages
                .iter()
                .flat_map(|message| &message.content)
                .find_map(|block| match block {
                    Content::Text { text }
                        if text.starts_with(
                            "Reference guidance from tickloop-artifacts:artifact-design",
                        ) =>
                    {
                        Some(text)
                    }
                    _ => None,
                })
                .unwrap()
        };
        assert!(!guidance(4).contains("REFERENCE_END"));
        assert!(guidance(5).contains("REFERENCE_END"));
        assert!(guidance(5).contains("preserve these arguments"));
        assert_eq!(
            guidance(5)
                .matches("Reference file: references/runtime.md")
                .count(),
            1
        );
    }
    assert_eq!(
        service
            .control
            .requests
            .lock()
            .unwrap()
            .iter()
            .filter(|request| request["body"]["method"] == "resources/read")
            .count(),
        2
    );
    agent.shutdown().await;
}

struct CompactionGateModel {
    inner: ScriptedModel,
    started: tokio::sync::Notify,
    release: tokio::sync::Notify,
}

#[async_trait]
impl ModelClient for CompactionGateModel {
    fn identity(&self) -> ModelIdentity {
        self.inner.identity()
    }
    async fn generate_title(
        &self,
        request: ModelRequest,
        events: tokio::sync::mpsc::UnboundedSender<ModelDelta>,
        cancel: CancellationToken,
    ) -> Result<ModelOutput, ModelError> {
        self.inner.generate_title(request, events, cancel).await
    }
    async fn complete(
        &self,
        request: ModelRequest,
        events: tokio::sync::mpsc::UnboundedSender<ModelDelta>,
        cancel: CancellationToken,
    ) -> Result<ModelOutput, ModelError> {
        if request.tools.is_empty() {
            self.started.notify_one();
            tokio::select! {
                () = cancel.cancelled() => return Err(ModelError::Cancelled),
                () = self.release.notified() => {},
            }
        }
        self.inner.complete(request, events, cancel).await
    }
}

#[tokio::test]
async fn remote_skill_source_or_policy_change_during_compaction_refreshes_the_effective_context_and_catalog()
 {
    for change in ["policy", "manifest", "account", "revoked"] {
        let service = ArtifactService::start().await;
        service.control.skills_enabled.store(true, Ordering::SeqCst);
        let directory = tempfile::tempdir().unwrap();
        let model = Arc::new(CompactionGateModel {
            inner: ScriptedModel::new(vec![
                call(
                    "load",
                    "Skill",
                    json!({"skill":"tickloop-artifacts:artifact-design"}),
                ),
                text(&"older material ".repeat(2000)),
                text("A compact summary"),
                text("continued"),
            ]),
            started: tokio::sync::Notify::new(),
            release: tokio::sync::Notify::new(),
        });
        let agent = signed_in_agent(&service, directory.path(), model.clone()).await;
        let chat = agent.create_chat(directory.path().into()).await.unwrap();
        submit_and_wait(&agent, chat, "Build a page").await;
        agent.set_context_window(chat, 20_000).await.unwrap();
        agent
            .submit(chat, None, "Continue with the page".into())
            .await
            .unwrap();
        tokio::time::timeout(WAIT, model.started.notified())
            .await
            .unwrap();
        match change {
            "policy" => {
                agent
                    .set_permission_rules(
                        chat,
                        vec![tickloop_agent_protocol::PermissionRule {
                            id: uuid::Uuid::new_v4(),
                            decision: tickloop_agent_protocol::Decision::Deny,
                            tool: "Skill".into(),
                            specifier: None,
                            input: None,
                            loop_id: None,
                            persistent: false,
                        }],
                    )
                    .await
                    .unwrap();
            }
            "manifest" => {
                service.control.skill_revision.store(1, Ordering::SeqCst);
            }
            "account" => {
                service.control.other_account.store(true, Ordering::SeqCst);
                service.login(directory.path()).await;
            }
            "revoked" => {
                service
                    .control
                    .skills_enabled
                    .store(false, Ordering::SeqCst);
            }
            _ => unreachable!(),
        }
        model.release.notify_one();
        wait_for_completion(&agent, chat).await;
        {
            let requests = model.inner.requests.lock().unwrap();
            let request = requests.last().unwrap();
            assert_eq!(
                request
                    .tools
                    .iter()
                    .find(|tool| tool.name == "Skill")
                    .unwrap()
                    .description
                    .contains("tickloop-artifacts:artifact-design"),
                matches!(change, "manifest" | "account"),
                "{change}"
            );
            assert!(
                !serde_json::to_string(&request.messages)
                    .unwrap()
                    .contains("Page guidance fixture for artifact-design"),
                "{change}"
            );
        }
        agent.shutdown().await;
    }
}

#[tokio::test]
async fn remote_skill_post_compaction_refresh_can_be_cancelled_while_the_server_is_blocked() {
    let service = ArtifactService::start().await;
    service.control.skills_enabled.store(true, Ordering::SeqCst);
    let directory = tempfile::tempdir().unwrap();
    let model = Arc::new(CompactionGateModel {
        inner: ScriptedModel::new(vec![
            call(
                "load",
                "Skill",
                json!({"skill":"tickloop-artifacts:artifact-design"}),
            ),
            text(&"older material ".repeat(2000)),
            text("A compact summary"),
            text("must not run"),
        ]),
        started: tokio::sync::Notify::new(),
        release: tokio::sync::Notify::new(),
    });
    let agent = signed_in_agent(&service, directory.path(), model.clone()).await;
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    submit_and_wait(&agent, chat, "Build a page").await;
    agent.set_context_window(chat, 20_000).await.unwrap();
    let receipt = agent
        .submit(chat, None, "Continue with the page".into())
        .await
        .unwrap();
    tokio::time::timeout(WAIT, model.started.notified())
        .await
        .unwrap();
    service
        .control
        .hold_status_response
        .store(true, Ordering::SeqCst);
    model.release.notify_one();
    tokio::time::timeout(WAIT, service.control.status_response_started.notified())
        .await
        .unwrap();
    agent.interrupt(chat).await.unwrap();
    let cancelled = tokio::time::timeout(
        Duration::from_secs(5),
        wait_for_loop_state(&agent, chat, receipt.loop_id, LoopState::Interrupted),
    )
    .await;
    service
        .control
        .hold_status_response
        .store(false, Ordering::SeqCst);
    service.control.release_status_response.notify_one();
    cancelled.expect("cancellation waited for the blocked service refresh");
    assert_eq!(model.inner.requests.lock().unwrap().len(), 3);
    agent.shutdown().await;
}
