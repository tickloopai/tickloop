use std::{
    sync::{
        Arc, Mutex,
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
        HostCallResult, HostCallScope, HostEffect, HostToolPermission, HostToolProvider,
        PreparedHostCall, WorkspaceHostContext, WorkspaceHostName, WorkspaceIntegration,
    },
    mcp_tools::McpHostToolProvider,
};
use tokio_util::sync::CancellationToken;

use tickloop_agent_test_support::{ScriptedModel, call, config, text};

#[path = "support/artifact_service.rs"]
pub(super) mod artifact_service;
use artifact_service::{
    ArtifactService, CREATED_ARTIFACT, FAILING_RESOURCE, FORBIDDEN_RESOURCE, GARBLED_RESOURCE,
};

use crate::support::{assert_extends, guidance_in_effect, mentions};

const WAIT: Duration = Duration::from_secs(25);

#[derive(Clone, Default)]
struct WarningLog(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for WarningLog {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[tokio::test]
async fn mcp_directory_refresh_reports_failures_and_keeps_chats_running() {
    use tracing::instrument::WithSubscriber;

    for (method, error_class, host_managed) in [
        ("server/discover", "session_unavailable", false),
        ("tools/list", "tool_listing_unavailable", false),
        ("server/discover", "session_unavailable", true),
        ("tools/list", "tool_listing_unavailable", true),
    ] {
        let service = ArtifactService::start().await;
        let directory = tempfile::tempdir().unwrap();
        let provider = Arc::new(if host_managed {
            McpHostToolProvider::with_credentials(
                &service.origin,
                "user-one",
                Arc::new(HostTokens {
                    token: Mutex::new("test-service-token-0".into()),
                    invalidations: AtomicUsize::new(0),
                }),
                None,
            )
            .unwrap()
        } else {
            McpHostToolProvider::new(service.login(directory.path()).await, None)
        });
        provider.refresh(CancellationToken::new()).await.unwrap();
        assert_eq!(provider.definitions().len(), 4);

        if method == "server/discover" {
            service
                .control
                .reject_discovery
                .store(true, Ordering::SeqCst);
        } else {
            service
                .control
                .reject_tool_listing
                .store(true, Ordering::SeqCst);
        }
        let log = WarningLog::default();
        let writer = log.clone();
        let subscriber = tracing_subscriber::fmt()
            .without_time()
            .with_ansi(false)
            .with_max_level(tracing::Level::WARN)
            .with_writer(move || writer.clone())
            .finish();
        let error = provider
            .refresh(CancellationToken::new())
            .with_subscriber(subscriber)
            .await
            .expect_err("a failed MCP directory must not look like an empty successful load");
        let warning = String::from_utf8(log.0.lock().unwrap().clone()).unwrap();
        assert!(warning.contains("WARN"), "{warning}");
        assert!(
            warning.contains("MCP tool directory refresh failed"),
            "{warning}"
        );
        for diagnostic in [&warning, &error] {
            assert!(diagnostic.contains(&service.origin), "{diagnostic}");
            assert!(diagnostic.contains(error_class), "{diagnostic}");
            assert!(!diagnostic.contains("test-service-token-0"), "{diagnostic}");
            assert!(!diagnostic.contains("Method not found"), "{diagnostic}");
        }
        assert!(provider.definitions().is_empty());
        assert_eq!(
            provider.service_connections()["artifacts"]["authentication"],
            "signed_in"
        );
        assert_eq!(
            provider.service_connections()["artifacts"]["tool_directory"],
            "unavailable"
        );

        let model = Arc::new(ScriptedModel::new(vec![text("Still running.")]));
        let agent =
            Agent::open_embedded(directory.path().join("state"), config(), model.clone()).unwrap();
        agent.install_host_tools(provider.clone()).unwrap();
        let chat = agent.create_chat(directory.path().into()).await.unwrap();
        submit_and_wait(&agent, chat, "Keep going without Artifacts").await;
        assert!(
            !tool_names(&model.requests.lock().unwrap()[0])
                .iter()
                .any(|name| name.starts_with("Artifact"))
        );
        agent.shutdown().await;

        service
            .control
            .reject_discovery
            .store(false, Ordering::SeqCst);
        service
            .control
            .reject_tool_listing
            .store(false, Ordering::SeqCst);
        provider.refresh(CancellationToken::new()).await.unwrap();
        assert_eq!(provider.definitions().len(), 4);
    }
}

#[tokio::test]
async fn mcp_resources_list_and_read_work_without_the_skills_extension_and_follow_cursors() {
    let service = ArtifactService::start().await;
    service
        .control
        .resources_enabled
        .store(true, Ordering::SeqCst);
    let directory = tempfile::tempdir().unwrap();
    let model = Arc::new(ScriptedModel::new(vec![
        call("list", "ListMcpResourcesTool", json!({})),
        call(
            "next",
            "ListMcpResourcesTool",
            json!({"server":"tickloop-artifacts","cursor":"next-page"}),
        ),
        call(
            "read",
            "ReadMcpResourceTool",
            json!({"server":"tickloop-artifacts","uri":"fixture://tickloop-artifacts/info"}),
        ),
        text("done"),
    ]));
    let agent = signed_in_agent(&service, directory.path(), model.clone()).await;
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    submit_and_wait(&agent, chat, "Find and read the MCP resources").await;
    assert!(
        agent
            .snapshot(chat)
            .await
            .unwrap()
            .invoked_skills
            .is_empty()
    );
    {
        let requests = model.requests.lock().unwrap();
        let first = &requests[0];
        assert!(
            first
                .tools
                .iter()
                .any(|tool| tool.name == "ListMcpResourcesTool")
        );
        assert!(
            first
                .tools
                .iter()
                .any(|tool| tool.name == "ReadMcpResourceTool")
        );
        assert!(!first.tools.iter().any(|tool| {
            tool.name == "Skill"
                && tool
                    .description
                    .contains("tickloop-artifacts:artifact-design")
        }));
        let last = serde_json::to_string(&requests.last().unwrap().messages).unwrap();
        for expected in [
            "fixture://tickloop-artifacts/info",
            "fixture://tickloop-artifacts/details",
            "Ordinary MCP resource fixture",
            "next-page",
        ] {
            assert!(last.contains(expected), "{expected}");
        }
    }
    let methods = service
        .control
        .requests
        .lock()
        .unwrap()
        .iter()
        .map(|request| request["body"]["method"].as_str().unwrap_or("").to_owned())
        .collect::<Vec<_>>();
    assert_eq!(
        methods
            .iter()
            .filter(|method| *method == "resources/list")
            .count(),
        2
    );
    assert_eq!(
        methods
            .iter()
            .filter(|method| *method == "resources/read")
            .count(),
        1
    );
    assert!(!methods.iter().any(|method| method == "skills/list"));
    agent.shutdown().await;
}

#[tokio::test]
async fn failed_resource_reads_say_why_without_echoing_the_service() {
    let service = ArtifactService::start().await;
    service
        .control
        .resources_enabled
        .store(true, Ordering::SeqCst);
    let directory = tempfile::tempdir().unwrap();
    let missing = "fixture://tickloop-artifacts/missing";
    let reads = [
        ("missing", missing),
        ("failing", FAILING_RESOURCE),
        ("forbidden", FORBIDDEN_RESOURCE),
        ("garbled", GARBLED_RESOURCE),
    ];
    let mut script = reads
        .iter()
        .map(|(id, uri)| {
            call(
                id,
                "ReadMcpResourceTool",
                json!({"server":"tickloop-artifacts","uri":uri}),
            )
        })
        .collect::<Vec<_>>();
    script.push(text("done"));
    let model = Arc::new(ScriptedModel::new(script));
    let agent = signed_in_agent(&service, directory.path(), model.clone()).await;
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    submit_and_wait(&agent, chat, "Read the resources").await;
    {
        let requests = model.requests.lock().unwrap();
        let last = requests.last().unwrap();
        for (id, expected) in [
            ("missing", format!("MCP resource not found: {missing}")),
            ("failing", "MCP server error -32603".into()),
            ("forbidden", "MCP access forbidden (HTTP 403)".into()),
            ("garbled", "invalid MCP response".into()),
        ] {
            let result = tool_result_text(last, id);
            assert!(result.contains(&expected), "{id}: {result}");
            for echoed in [
                "test-service-token",
                "Resource not found",
                "Internal error",
                "ignore",
            ] {
                assert!(!result.contains(echoed), "{id}: {result}");
            }
        }
    }
    agent.shutdown().await;
}

#[tokio::test]
async fn mcp_resource_listing_obeys_denials_and_rejects_ambiguous_cursors_before_fetching() {
    for denied in [false, true] {
        let service = ArtifactService::start().await;
        service
            .control
            .resources_enabled
            .store(true, Ordering::SeqCst);
        let directory = tempfile::tempdir().unwrap();
        let model = Arc::new(ScriptedModel::new(vec![
            call(
                "wrong-server",
                "ListMcpResourcesTool",
                json!({"server":"other"}),
            ),
            call(
                "cursor",
                "ListMcpResourcesTool",
                json!({"cursor":"next-page"}),
            ),
            call(
                "list",
                "ListMcpResourcesTool",
                json!({"server":"tickloop-artifacts"}),
            ),
            text("done"),
        ]));
        let agent = signed_in_agent(&service, directory.path(), model.clone()).await;
        let chat = agent.create_chat(directory.path().into()).await.unwrap();
        if denied {
            agent
                .set_permission_rules(
                    chat,
                    vec![tickloop_agent_protocol::PermissionRule {
                        id: uuid::Uuid::new_v4(),
                        decision: tickloop_agent_protocol::Decision::Deny,
                        tool: "ListMcpResourcesTool".into(),
                        specifier: None,
                        input: None,
                        loop_id: None,
                        persistent: false,
                    }],
                )
                .await
                .unwrap();
        }
        submit_and_wait(&agent, chat, "List the resources").await;
        {
            let requests = service.control.requests.lock().unwrap();
            assert_eq!(
                requests
                    .iter()
                    .filter(|request| request["body"]["method"] == "resources/list")
                    .count(),
                usize::from(!denied)
            );
        }
        agent.shutdown().await;
    }
}

#[tokio::test]
async fn remote_resource_reads_do_not_activate_skills_and_repeated_reads_return_content() {
    let service = ArtifactService::start().await;
    service.control.skills_enabled.store(true, Ordering::SeqCst);
    let directory = tempfile::tempdir().unwrap();
    let model = Arc::new(ScriptedModel::new(vec![
        call(
            "raw-body",
            "ReadMcpResourceTool",
            json!({"server":"tickloop-artifacts","uri":"skill://tickloop-artifacts/artifact-design/SKILL.md"}),
        ),
        call(
            "reference",
            "ReadMcpResourceTool",
            json!({"server":"tickloop-artifacts","uri":"skill://tickloop-artifacts/artifact-design/references/guide.md"}),
        ),
        call(
            "repeat",
            "ReadMcpResourceTool",
            json!({"server":"tickloop-artifacts","uri":"skill://tickloop-artifacts/artifact-design/references/guide.md"}),
        ),
        call(
            "old-resource-input",
            "Skill",
            json!({"skill":"tickloop-artifacts:artifact-design","resource":"references/guide.md"}),
        ),
        text("done"),
    ]));
    let agent = signed_in_agent(&service, directory.path(), model.clone()).await;
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
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
    submit_and_wait(&agent, chat, "Read the published reference files").await;
    assert!(
        agent
            .snapshot(chat)
            .await
            .unwrap()
            .invoked_skills
            .is_empty()
    );
    {
        let requests = model.requests.lock().unwrap();
        let last = requests.last().unwrap();
        let results = last
            .messages
            .iter()
            .flat_map(|message| &message.content)
            .filter_map(|block| match block {
                Content::ToolResult {
                    tool_use_id,
                    content,
                    is_error,
                } => Some((tool_use_id.as_str(), (content.text(), *is_error))),
                _ => None,
            })
            .collect::<std::collections::HashMap<_, _>>();
        assert!(!results["raw-body"].1);
        assert!(
            results["raw-body"]
                .0
                .contains("Page guidance fixture for artifact-design")
        );
        assert!(!results["reference"].1);
        assert!(!results["repeat"].1);
        assert!(
            results["repeat"]
                .0
                .contains("is unchanged since it was read earlier in this conversation")
        );
        assert!(
            results["reference"]
                .0
                .contains("Reference fixture: preserve data units")
        );
        assert!(results["old-resource-input"].1);
        assert!(results["old-resource-input"].0.contains("resource"));
        let skill = last.tools.iter().find(|tool| tool.name == "Skill").unwrap();
        assert!(skill.input_schema["properties"].get("resource").is_none());
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
async fn remote_resource_reads_reject_unknown_sources_paths_and_obey_denials() {
    for denied in [false, true] {
        let service = ArtifactService::start().await;
        service.control.skills_enabled.store(true, Ordering::SeqCst);
        let directory = tempfile::tempdir().unwrap();
        let model = Arc::new(ScriptedModel::new(vec![
            call(
                "wrong-server",
                "ReadMcpResourceTool",
                json!({"server":"other","uri":"skill://tickloop-artifacts/artifact-design/references/guide.md"}),
            ),
            call(
                "traversal",
                "ReadMcpResourceTool",
                json!({"server":"tickloop-artifacts","uri":"skill://tickloop-artifacts/artifact-design/../doc/SKILL.md"}),
            ),
            call(
                "unlisted",
                "ReadMcpResourceTool",
                json!({"server":"tickloop-artifacts","uri":"skill://tickloop-artifacts/artifact-design/references/missing.md"}),
            ),
            call(
                "reference",
                "ReadMcpResourceTool",
                json!({"server":"tickloop-artifacts","uri":"skill://tickloop-artifacts/artifact-design/references/guide.md"}),
            ),
            text("done"),
        ]));
        let agent = signed_in_agent(&service, directory.path(), model.clone()).await;
        let chat = agent.create_chat(directory.path().into()).await.unwrap();
        if denied {
            agent
                .set_permission_rules(
                    chat,
                    vec![tickloop_agent_protocol::PermissionRule {
                        id: uuid::Uuid::new_v4(),
                        decision: tickloop_agent_protocol::Decision::Deny,
                        tool: "ReadMcpResourceTool".into(),
                        specifier: None,
                        input: None,
                        loop_id: None,
                        persistent: false,
                    }],
                )
                .await
                .unwrap();
        }
        submit_and_wait(&agent, chat, "Read the references").await;
        let snapshot = agent.snapshot(chat).await.unwrap();
        for id in ["wrong-server", "traversal", "unlisted"] {
            assert!(snapshot.messages.iter().flat_map(|message| &message.content).any(|block| matches!(block, Content::ToolResult {tool_use_id, is_error:true, ..} if tool_use_id == id)));
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
            2 * usize::from(!denied)
        );
        assert!(snapshot.invoked_skills.is_empty());
        agent.shutdown().await;
    }
}

#[tokio::test]
async fn remote_resource_reads_discard_responses_when_account_or_policy_changes() {
    for (change, tool) in [
        ("account", "ReadMcpResourceTool"),
        ("policy", "ReadMcpResourceTool"),
        ("account", "ListMcpResourcesTool"),
        ("policy", "ListMcpResourcesTool"),
    ] {
        let service = ArtifactService::start().await;
        service.control.skills_enabled.store(true, Ordering::SeqCst);
        service
            .control
            .hold_skill_response
            .store(true, Ordering::SeqCst);
        let directory = tempfile::tempdir().unwrap();
        let model = Arc::new(ScriptedModel::new(vec![
            call(
                "reference",
                tool,
                if tool == "ReadMcpResourceTool" {
                    json!({"server":"tickloop-artifacts","uri":"skill://tickloop-artifacts/artifact-design/references/guide.md"})
                } else {
                    json!({"server":"tickloop-artifacts"})
                },
            ),
            text("done"),
        ]));
        let agent = signed_in_agent(&service, directory.path(), model.clone()).await;
        let chat = agent.create_chat(directory.path().into()).await.unwrap();
        agent
            .submit(chat, "Read the reference".into())
            .await
            .unwrap();
        tokio::time::timeout(WAIT, service.control.skill_response_started.notified())
            .await
            .unwrap();
        match change {
            "account" => {
                service.control.other_account.store(true, Ordering::SeqCst);
                service.login(directory.path()).await;
            }
            "policy" => {
                agent
                    .set_permission_rules(
                        chat,
                        vec![tickloop_agent_protocol::PermissionRule {
                            id: uuid::Uuid::new_v4(),
                            decision: tickloop_agent_protocol::Decision::Deny,
                            tool: tool.into(),
                            specifier: None,
                            input: None,
                            loop_id: None,
                            persistent: false,
                        }],
                    )
                    .await
                    .unwrap();
            }
            _ => unreachable!(),
        }
        service.control.release_skill_response.notify_one();
        wait_for_completion(&agent, chat).await;
        let snapshot = agent.snapshot(chat).await.unwrap();
        assert!(snapshot.invoked_skills.is_empty());
        {
            let requests = model.requests.lock().unwrap();
            let last = serde_json::to_string(&requests.last().unwrap().messages).unwrap();
            assert!(
                !last.contains("Reference fixture: preserve data units"),
                "{change}"
            );
            assert!(
                !last.contains("fixture://tickloop-artifacts/info"),
                "{change}: {tool}"
            );
            assert!(last.contains("changed during"), "{change}: {tool}: {last}");
        }
        agent.shutdown().await;
    }
}

#[tokio::test]
async fn guidance_instructions_and_an_unchanged_reference_enter_history_once() {
    let service = ArtifactService::start().await;
    service.control.skills_enabled.store(true, Ordering::SeqCst);
    let directory = tempfile::tempdir().unwrap();
    let design_guide = "skill://tickloop-artifacts/artifact-design/references/guide.md";
    let diagram_guide = "skill://tickloop-artifacts/artifact-diagramming/references/guide.md";
    let model = Arc::new(ScriptedModel::new(vec![
        call(
            "design",
            "Skill",
            json!({"skill":"tickloop-artifacts:artifact-design"}),
        ),
        call(
            "design-guide",
            "ReadMcpResourceTool",
            json!({"server":"tickloop-artifacts","uri":design_guide}),
        ),
        call(
            "diagram",
            "Skill",
            json!({"skill":"tickloop-artifacts:artifact-diagramming"}),
        ),
        call(
            "diagram-guide",
            "ReadMcpResourceTool",
            json!({"server":"tickloop-artifacts","uri":diagram_guide}),
        ),
        call(
            "design-guide-again",
            "ReadMcpResourceTool",
            json!({"server":"tickloop-artifacts","uri":design_guide}),
        ),
        text("Designed."),
        call(
            "reuse",
            "Skill",
            json!({"skill":"tickloop-artifacts:artifact-design"}),
        ),
        text("Still designing."),
    ]));
    let agent = signed_in_agent(&service, directory.path(), model.clone()).await;
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    submit_and_wait(&agent, chat, "Make a visual page").await;
    submit_and_wait(&agent, chat, "Add a section").await;
    {
        let instructions = artifact_service::instructions(&service.control);
        let requests = model.requests.lock().unwrap();
        assert_eq!(requests.len(), 8);
        for (earlier, later) in requests.iter().zip(requests.iter().skip(1)) {
            assert_extends(earlier, later);
        }
        for request in requests.iter() {
            assert_eq!(mentions(request, "# MCP Server Instructions"), 1);
            assert_eq!(
                mentions(
                    request,
                    &serde_json::to_string(&instructions).unwrap()[1..40]
                ),
                1
            );
        }
        let last = &requests[7];
        assert_eq!(
            mentions(last, "Page guidance fixture for artifact-design"),
            1
        );
        assert_eq!(
            mentions(last, "Page guidance fixture for artifact-diagramming"),
            1
        );
        // Like Claude Code's Read, only the same resource unchanged is elided: another URI with
        // the same text is still read in full.
        assert_eq!(mentions(last, "Reference fixture: preserve data units"), 2);
        let repeated = tool_result_text(last, "design-guide-again");
        assert!(
            repeated.contains(design_guide) && repeated.contains("unchanged since it was read"),
            "{repeated}"
        );
        assert!(!repeated.contains("Reference fixture"), "{repeated}");
        let reused = tool_result_text(last, "reuse");
        assert!(reused.contains("Already loaded"), "{reused}");
        assert!(reused.contains("already in this conversation"), "{reused}");
    }
    let reads = service
        .control
        .requests
        .lock()
        .unwrap()
        .iter()
        .filter(|request| request["body"]["method"] == "resources/read")
        .map(|request| {
            request["body"]["params"]["uri"]
                .as_str()
                .unwrap()
                .to_owned()
        })
        .collect::<Vec<_>>();
    assert_eq!(reads.len(), 4, "{reads:?}");
    assert_eq!(
        reads
            .iter()
            .filter(|uri| uri.as_str() == design_guide)
            .count(),
        1
    );
    agent.shutdown().await;
}

#[tokio::test]
async fn changed_or_withdrawn_server_instructions_are_appended_after_the_old_ones() {
    let service = ArtifactService::start().await;
    let directory = tempfile::tempdir().unwrap();
    let model = Arc::new(ScriptedModel::new(vec![
        text("First."),
        text("Second."),
        text("Third."),
    ]));
    let agent = signed_in_agent(&service, directory.path(), model.clone()).await;
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    submit_and_wait(&agent, chat, "Make a doc").await;
    service
        .control
        .instructions_revision
        .store(1, Ordering::SeqCst);
    submit_and_wait(&agent, chat, "Make another").await;
    service.client(directory.path()).logout().await.unwrap();
    submit_and_wait(&agent, chat, "And another").await;
    let requests = model.requests.lock().unwrap();
    assert_eq!(requests.len(), 3);
    assert_extends(&requests[0], &requests[1]);
    assert_extends(&requests[1], &requests[2]);
    assert_eq!(mentions(&requests[0], "# MCP Server Instructions"), 1);
    assert_eq!(mentions(&requests[1], "# MCP Server Instructions"), 2);
    assert_eq!(mentions(&requests[1], "(instructions revision 1)"), 1);
    assert_eq!(mentions(&requests[2], "(instructions revision 1)"), 1);
    assert_eq!(
        mentions(&requests[2], "Their instructions above no longer apply"),
        1
    );
}

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
            "ReadMcpResourceTool",
            json!({"server":"tickloop-artifacts","uri":"skill://tickloop-artifacts/artifact-design/references/guide.md"}),
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
            "dataviz",
            "doc",
            "artifact-slides",
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
async fn remote_skill_arguments_do_not_absorb_resource_reads_and_identical_repeats_are_elided() {
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
            "ReadMcpResourceTool",
            json!({"server":"tickloop-artifacts","uri":"skill://tickloop-artifacts/artifact-design/references/guide.md"}),
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
            .rev()
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
        assert!(!changed.contains("Reference fixture: preserve data units"));
        assert!(
            serde_json::to_string(&requests[3].messages)
                .unwrap()
                .contains("Reference fixture: preserve data units")
        );
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
    for change in ["account", "manifest", "policy", "revoked", "source"] {
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
                "ReadMcpResourceTool",
                json!({"server":"tickloop-artifacts","uri":"skill://tickloop-artifacts/artifact-design/references/guide.md"}),
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
                !guidance_in_effect(
                    requests.last().unwrap(),
                    "tickloop-artifacts:artifact-design"
                ),
                "{change}"
            );
            assert_extends(&requests[requests.len() - 2], requests.last().unwrap());
        }
        agent.shutdown().await;
    }
}

#[tokio::test]
async fn resumed_remote_skills_are_revalidated_before_the_first_model_request() {
    for change in ["unchanged", "account", "manifest", "policy", "revoked"] {
        let service = ArtifactService::start().await;
        service.control.skills_enabled.store(true, Ordering::SeqCst);
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("state");
        let account = service.login(directory.path()).await;
        let agent = Agent::open_embedded(root.clone(), config(), Arc::new(ScriptedModel::new(vec![call("load", "Skill", json!({"skill":"tickloop-artifacts:artifact-design"})), call("reference", "ReadMcpResourceTool", json!({"server":"tickloop-artifacts","uri":"skill://tickloop-artifacts/artifact-design/references/guide.md"})), text("done")]))).unwrap();
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
        restored.resume_chat(chat).await.unwrap();
        if change == "policy" {
            restored
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

        restored
            .submit(chat, "Continue the work".into())
            .await
            .unwrap();
        wait_for_completion(&restored, chat).await;
        {
            let requests = model.requests.lock().unwrap();
            let first = serde_json::to_string(&requests[0].messages).unwrap();
            assert_eq!(
                guidance_in_effect(&requests[0], "tickloop-artifacts:artifact-design"),
                change == "unchanged",
                "{change}"
            );
            assert_eq!(
                mentions(&requests[0], "Page guidance fixture for artifact-design"),
                1,
                "a resumed Chat keeps its announced guidance without announcing it again: {change}"
            );
            assert!(
                first.contains("Reference fixture: preserve data units"),
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
    agent.submit(chat, "Build a page".into()).await.unwrap();
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
    agent.submit(chat, "Prepare a page".into()).await.unwrap();
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
async fn remote_skill_and_resource_digest_mismatches_never_enter_context() {
    let service = ArtifactService::start().await;
    service.control.skills_enabled.store(true, Ordering::SeqCst);
    service.control.tamper_skill.store(true, Ordering::SeqCst);
    let directory = tempfile::tempdir().unwrap();
    let model = Arc::new(ScriptedModel::new(vec![
        call(
            "reference-first",
            "ReadMcpResourceTool",
            json!({"server":"tickloop-artifacts","uri":"skill://tickloop-artifacts/artifact-design/references/guide.md"}),
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
        2
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

async fn wait_for_loop_state(agent: &Agent, chat: ChatId, state: LoopState) {
    eventually(async || agent.snapshot(chat).await.unwrap().chat_loop.state == state).await;
}

async fn wait_for_completion(agent: &Agent, chat: ChatId) {
    eventually(async || agent.snapshot(chat).await.unwrap().chat_loop.state == LoopState::Idle)
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
        .link_artifact(
            chat,
            &artifact_tick(service, "doc"),
            tickloop_agent_protocol::ArtifactFeedbackHistory::All,
        )
        .await
        .unwrap();
    let loop_id = agent.snapshot(chat).await.unwrap().chat_loop.id;
    (agent, chat, loop_id)
}

#[tokio::test]
async fn directory_presents_exactly_the_three_artifact_tools_and_the_host_upload_as_built_ins() {
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
        [
            "ArtifactComment",
            "ArtifactEdit",
            "ArtifactRead",
            "ArtifactUpload"
        ]
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
    agent.submit(chat, "Inspect".into()).await.unwrap();
    wait_for_completion(&agent, chat).await;
    agent.shutdown().await;
    let requests = model.requests.lock().unwrap();
    let names = tool_names(&requests[0]);
    for name in [
        "ArtifactRead",
        "ArtifactEdit",
        "ArtifactComment",
        "ArtifactUpload",
    ] {
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
    assert!(
        provider
            .refresh(CancellationToken::new())
            .await
            .unwrap_err()
            .contains("directory_rejected")
    );
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
    assert!(
        provider
            .refresh(CancellationToken::new())
            .await
            .unwrap_err()
            .contains("directory_rejected")
    );
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
    agent.submit(chat, "Tighten it".into()).await.unwrap();
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
async fn a_base62_write_receipt_admits_only_the_same_artifact_in_either_id_form() {
    let service = ArtifactService::start().await;
    service.control.compact_ids.store(true, Ordering::SeqCst);
    *service.control.publication.lock().unwrap() =
        Some(json!({"audience":"private","membership_revision":0}));
    let directory = tempfile::tempdir().unwrap();
    let provider = McpHostToolProvider::new(service.login(directory.path()).await, None);
    provider.refresh(CancellationToken::new()).await.unwrap();
    let chat = uuid::Uuid::new_v4();
    let uuid = "e1b85a39-a57b-4c28-a8a5-ae9233aead3e";
    let short = "6rvWPvOnhVABug7llbHb26";
    let prepare = async |id: &str| {
        provider.prepare_call(chat, "ArtifactEdit", &json!({"action":"rename","artifact_id":id,"title":"Renamed","expected_title":"Fixture"}), CancellationToken::new()).await.unwrap()
    };
    let first = prepare(uuid).await;
    assert_eq!(first.permission(), HostToolPermission::PrivateArtifactWrite);
    let HostCallResult::CompletedWithEffect(result, effect) = provider
        .execute_prepared(chat, first, CancellationToken::new())
        .await
    else {
        panic!("a confirmed rename reports its effect");
    };
    assert!(!result.is_error);
    let HostEffect::RenamedArtifact { instance, .. } = *effect else {
        panic!("a rename effect");
    };
    assert_eq!(
        instance,
        tick_protocol::InstanceRef::artifact(&service.origin, uuid).unwrap()
    );
    assert_eq!(
        prepare(uuid).await.permission(),
        HostToolPermission::ArtifactContinuation
    );
    assert_eq!(
        prepare(short).await.permission(),
        HostToolPermission::ArtifactContinuation
    );
    assert_eq!(
        prepare("11111111-1111-4111-8111-111111111111")
            .await
            .permission(),
        HostToolPermission::PrivateArtifactWrite
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
        .submit(chat, "Rename it to Q3 plan".into())
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
    agent.submit(chat, "Plan a reply".into()).await.unwrap();
    let reply = loop {
        if let EventKind::ToolCompleted {
            call_id, result, ..
        } = tickloop_agent_test_support::next_event(&mut events)
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
        .submit(chat, "Delegate tool inspection".into())
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
            .submit(parent, "Delegate the reply".into())
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
                EventKind::ToolCompleted {
                    call_id, result, ..
                } if call_id == "child-reply" => child_result = Some(result),
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
            .submit(parent, "Delegate the edit".into())
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
            if let EventKind::ToolCompleted {
                call_id, result, ..
            } = event.kind
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
async fn a_feedback_delegation_keeps_the_receiving_account_after_login_changes() {
    let service = ArtifactService::start().await;
    let directory = tempfile::tempdir().unwrap();
    service.request_feedback("doc", "delegated-feedback");
    let model = Arc::new(GatedDelegatedModel {
        entered: tokio::sync::Notify::new(),
        release: tokio::sync::Notify::new(),
        child_turns: AtomicUsize::new(0),
        parent_turns: AtomicUsize::new(0),
    });
    let (agent, parent, _loop_id) = linked_chat(&service, directory.path(), model.clone()).await;
    tokio::time::timeout(WAIT, model.entered.notified())
        .await
        .unwrap();
    let child = agent.subagents(parent).await.unwrap()[0].child_chat_id;
    let child_snapshot = agent.snapshot(child).await.unwrap();
    let ceiling = child_snapshot.delegation.unwrap().ceiling;
    for name in [
        "ArtifactRead",
        "ArtifactEdit",
        "ArtifactComment",
        "ArtifactUpload",
    ] {
        assert!(ceiling.host_tools[name].iter().all(|authority| matches!(authority,
            tickloop_agent_protocol::HostToolAuthority::Mcp { server_id, account_id } if server_id == &service.origin && account_id == "user-one")));
    }
    let mut events = agent.subscribe();
    service.control.other_account.store(true, Ordering::SeqCst);
    service.login(directory.path()).await;
    model.release.notify_one();
    tokio::time::timeout(WAIT, async {
        loop {
            if let EventKind::ToolCompleted {
                call_id, result, ..
            } = tickloop_agent_test_support::next_event(&mut events)
                .await
                .kind
                && call_id == "frozen-write"
            {
                assert!(result.is_error);
                break;
            }
        }
    })
    .await
    .unwrap();
    wait_for_loop_state(&agent, parent, LoopState::Failed).await;
    assert!(service.tool_calls().is_empty());
    agent.shutdown().await;
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
    assert!(provider.refresh(CancellationToken::new()).await.is_err());
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
        .submit(chat, "Inspect native tool inventory".into())
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
        "TickCall",
        "TickRead",
        "TickManage",
    ] {
        assert!(tool_names(&requests[0]).contains(&name), "{name}");
    }
    let prompt = serde_json::to_string(&requests[0].messages).unwrap();
    assert!(prompt.contains("Tool absence is not evidence of being signed out"));
    assert!(
        prompt.contains("Operate artifacts with ArtifactRead, ArtifactEdit and ArtifactComment")
    );
    assert!(prompt.contains("authentication is signed_out, call ArtifactLogin"));
    assert!(prompt.contains("Tickloop Artifacts' own MCP server instructions"));
    assert!(!prompt.contains("use ArtifactEdit to create the matching type"));
    assert!(!tool_names(&requests[0]).contains(&"ArtifactLogin"));
    for name in ["TickCall", "TickRead"] {
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
    agent.submit(chat, "Check".into()).await.unwrap();
    wait_for_completion(&agent, chat).await;
    assert!(
        !tool_names(&model.requests.lock().unwrap()[0]).contains(&"ArtifactEdit"),
        "tools appear only after login"
    );
    service.login(directory.path()).await;
    let mut events = agent.subscribe();
    agent
        .submit(chat, "Replace the document".into())
        .await
        .unwrap();
    let permission = next_permission(&mut events).await;
    assert_eq!(permission.input["content"], "# Private");
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
    agent.submit(chat, "Replace".into()).await.unwrap();
    let permission = next_permission(&mut events).await;
    service.client(directory.path()).logout().await.unwrap();
    agent
        .answer_permission(chat, permission.id, true)
        .await
        .unwrap();
    wait_for_completion(&agent, chat).await;
    assert!(service.tool_calls().is_empty());
    {
        let requests = model.requests.lock().unwrap();
        assert!(!tool_names(&requests[1]).contains(&"ArtifactEdit"));
        assert!(tool_result_text(&requests[1], "replace").contains(NOTHING_SENT));
    }
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
    let rejection = provider
        .revalidate_call(chat, &prepared, CancellationToken::new())
        .await
        .unwrap_err();
    assert!(rejection.contains(NOTHING_SENT), "{rejection}");
    let cancelled = CancellationToken::new();
    cancelled.cancel();
    let preparation = provider
        .prepare_call(
            chat,
            "ArtifactEdit",
            &json!({"action":"replace","artifact_id":"doc","expected_revision":3,"content":"initial"}),
            cancelled.clone(),
        )
        .await
        .unwrap_err();
    assert!(preparation.contains(NOTHING_SENT), "{preparation}");
    match provider.execute_prepared(chat, prepared, cancelled).await {
        HostCallResult::NotDispatched(message) => {
            assert!(message.contains(NOTHING_SENT), "{message}");
        }
        _ => panic!("a cancelled call must not be dispatched"),
    }
    assert!(service.tool_calls().is_empty());
}

const NOTHING_SENT: &str = "Nothing was sent to Tickloop Artifacts and no artifact changed";

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
    agent.submit(chat, "Replace".into()).await.unwrap();
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

    async fn prepare_call_in_scope(
        &self,
        chat_id: tickloop_agent_protocol::ChatId,
        scope: &HostCallScope<'_>,
        name: &str,
        input: &Value,
        cancel: CancellationToken,
    ) -> Result<PreparedHostCall, String> {
        let prepared = self.prepare_call(chat_id, name, input, cancel).await?;
        Ok(match scope.context {
            Some(context) => prepared.with_payload(context.clone()),
            None => prepared,
        })
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
        folder: directory.path().into(),
        workspace_id: Some("work-context".into()),
        interaction: None,
    };
    let local_files = tickloop_agent_tools::Tools::new(directory.path().join("chat"))
        .local_file_reader(
            &tickloop_agent_tools::Workspace {
                folder: Some(directory.path().into()),
                additional_directories: Vec::new(),
            },
            Arc::new(NoLocalReads),
        );
    let prepared = provider
        .prepare_call_in_scope(
            uuid::Uuid::new_v4(),
            &HostCallScope {
                local_files: local_files.as_ref(),
                context: Some(&context),
            },
            "native_fixture",
            &json!({}),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    let captured = prepared
        .payload::<HostCallContext>()
        .expect("Native preparation receives its context");
    assert_eq!(captured.folder, context.folder);
    assert_eq!(captured.workspace_id, context.workspace_id);
}

#[tokio::test]
async fn a_renewed_artifacts_token_keeps_the_account_directory_and_dispatches_with_the_new_token() {
    let service = ArtifactService::start().await;
    let directory = tempfile::tempdir().unwrap();
    let account = service.login(directory.path()).await;
    let provider = McpHostToolProvider::new(account.clone(), None);
    provider.refresh(CancellationToken::new()).await.unwrap();
    expire_artifacts_tokens(directory.path());
    service
        .control
        .credential_revision
        .store(1, Ordering::SeqCst);
    let renewed = account.access_snapshot().await.unwrap().unwrap();
    assert_eq!(renewed.access_token, "test-service-token-1");
    let chat = ChatId::new_v4();
    provider
        .prepare_call(
            chat,
            "ArtifactRead",
            &json!({"action":"read","artifact_id":"doc"}),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    let edit =
        json!({"action":"replace","artifact_id":"doc","expected_revision":3,"content":"renewed"});
    let prepared = provider
        .prepare_call(chat, "ArtifactEdit", &edit, CancellationToken::new())
        .await
        .unwrap();
    expire_artifacts_tokens(directory.path());
    service
        .control
        .credential_revision
        .store(2, Ordering::SeqCst);
    account.access_snapshot().await.unwrap().unwrap();
    provider
        .revalidate_call(chat, &prepared, CancellationToken::new())
        .await
        .unwrap();
    let result = provider
        .execute_prepared(chat, prepared, CancellationToken::new())
        .await;
    assert!(matches!(
        result,
        HostCallResult::Completed(ToolResult {
            is_error: false,
            ..
        })
    ));
    let calls = service
        .requests()
        .into_iter()
        .filter(|request| request["body"]["method"] == "tools/call")
        .collect::<Vec<_>>();
    assert_eq!(calls.len(), 1);
    assert!(
        calls[0]["headers"]
            .as_str()
            .unwrap()
            .to_ascii_lowercase()
            .contains("authorization: bearer test-service-token-2")
    );

    service.control.other_account.store(true, Ordering::SeqCst);
    service.login(directory.path()).await;
    assert_eq!(
        provider
            .prepare_call(chat, "ArtifactEdit", &edit, CancellationToken::new())
            .await
            .err()
            .unwrap(),
        format!(
            "Artifact account changed before MCP preparation. {NOTHING_SENT}; the call can be retried with the same revision and hashes."
        )
    );
    assert_eq!(service.tool_calls().len(), 1);
}

#[tokio::test]
async fn an_expired_artifacts_token_is_renewed_when_a_call_needs_it() {
    let service = ArtifactService::start().await;
    let directory = tempfile::tempdir().unwrap();
    let account = service.login(directory.path()).await;
    let provider = McpHostToolProvider::new(account, None);
    provider.refresh(CancellationToken::new()).await.unwrap();
    let chat = ChatId::new_v4();
    let edit =
        json!({"action":"replace","artifact_id":"doc","expected_revision":3,"content":"renewed"});
    let expire_with_next_revision = |revision| {
        expire_artifacts_tokens(directory.path());
        service
            .control
            .credential_revision
            .store(revision, Ordering::SeqCst);
    };

    expire_with_next_revision(1);
    assert!(
        provider
            .call_authority("ArtifactEdit", &edit)
            .unwrap()
            .is_some()
    );
    provider
        .prepare_call(
            chat,
            "ArtifactRead",
            &json!({"action":"read","artifact_id":"doc"}),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    let prepared = provider
        .prepare_call(chat, "ArtifactEdit", &edit, CancellationToken::new())
        .await
        .unwrap();
    expire_with_next_revision(2);
    provider
        .revalidate_call(chat, &prepared, CancellationToken::new())
        .await
        .unwrap();
    expire_with_next_revision(3);
    let result = provider
        .execute_prepared(chat, prepared, CancellationToken::new())
        .await;
    assert!(matches!(
        result,
        HostCallResult::Completed(ToolResult {
            is_error: false,
            ..
        })
    ));
    let calls = service
        .requests()
        .into_iter()
        .filter(|request| request["body"]["method"] == "tools/call")
        .collect::<Vec<_>>();
    assert_eq!(calls.len(), 1);
    assert!(
        calls[0]["headers"]
            .as_str()
            .unwrap()
            .to_ascii_lowercase()
            .contains("authorization: bearer test-service-token-3")
    );

    expire_artifacts_tokens(directory.path());
    service
        .control
        .refuse_token_refresh
        .store(true, Ordering::SeqCst);
    assert_eq!(
        provider
            .prepare_call(chat, "ArtifactEdit", &edit, CancellationToken::new())
            .await
            .err()
            .unwrap(),
        format!(
            "Artifact login is no longer active. {NOTHING_SENT}; the call can be retried with the same revision and hashes."
        )
    );
    assert!(
        service
            .client(directory.path())
            .credential_snapshot()
            .unwrap()
            .is_none()
    );
    assert_eq!(service.tool_calls().len(), 1);
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
    assert!(provider.refresh(CancellationToken::new()).await.is_err());
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
        "signed_in",
        "a new token for the same account keeps the account's status"
    );
    service.control.other_account.store(true, Ordering::SeqCst);
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
async fn feedback_enters_the_linked_loop_once_without_a_completion_tool_or_private_claim() {
    let service = ArtifactService::start().await;
    let directory = tempfile::tempdir().unwrap();
    service.request_feedback("doc", "request-1");
    let model = Arc::new(ScriptedModel::new(vec![
        call(
            "reply",
            "ArtifactComment",
            json!({"action":"reply","artifact_id":"doc","thread_id":"thread-1","text":"Tightened."}),
        ),
        text("Handled."),
    ]));
    let (agent, chat, _loop_id) = linked_chat(&service, directory.path(), model.clone()).await;
    eventually(async || service.finished() == [("request-1".into(), "received".into())]).await;
    eventually(async || service.tool_calls().len() == 1).await;
    wait_for_loop_state(&agent, chat, LoopState::Idle).await;
    assert!(service.tool_calls()[0]["_meta"]["tickloop.ai/event-bridge"].is_null());
    let conversations = model
        .requests
        .lock()
        .unwrap()
        .iter()
        .map(|request| serde_json::to_string(&request.messages).unwrap())
        .collect::<Vec<_>>();
    assert!(conversations[0].contains("@tickloop feedback request-1 on artifact doc"));
    assert!(
        conversations
            .iter()
            .all(|conversation| !conversation.contains("fence-")
                && !conversation.contains("claim_id"))
    );
    let snapshot = agent.snapshot(chat).await.unwrap();
    assert!(!snapshot.chat_loop.event_sources[0].resume_required);
    assert_eq!(
        snapshot.chat_loop.event_sources[0].last_request_at_ms,
        Some(artifact_service::REQUEST_CREATED_AT_MS)
    );
    assert!(service.releases().is_empty());
    agent.shutdown().await;
}

#[tokio::test]
async fn base62_feedback_is_claimed_acknowledged_and_delivered_once() {
    let service = ArtifactService::start().await;
    service.control.compact_ids.store(true, Ordering::SeqCst);
    let feedback = "e1b85a39-a57b-4c28-a8a5-ae9233aead3e";
    service.request_feedback("doc", feedback);
    let directory = tempfile::tempdir().unwrap();
    let model = Arc::new(ScriptedModel::new(vec![text("Received.")]));
    let (agent, chat, _loop_id) = linked_chat(&service, directory.path(), model.clone()).await;
    eventually(async || service.finished() == [(feedback.into(), "received".into())]).await;
    // The service records the ack before the agent hands the staged feedback to its Loop, and a
    // fresh Loop is already Idle, so wait for the model call before waiting for the Loop to idle.
    eventually(async || !model.requests.lock().unwrap().is_empty()).await;
    wait_for_loop_state(&agent, chat, LoopState::Idle).await;
    assert_eq!(model.requests.lock().unwrap().len(), 1);
    assert_eq!(
        service
            .lifecycle_requests("POST /artifact/api/v1/agent/feedback/")
            .len(),
        2
    );
    assert!(!agent.snapshot(chat).await.unwrap().chat_loop.event_sources[0].resume_required);
    agent.shutdown().await;
}

#[tokio::test]
async fn multiple_feedback_inputs_are_received_without_execution_completion_states() {
    let service = ArtifactService::start().await;
    let directory = tempfile::tempdir().unwrap();
    service.request_feedback("doc", "request-1");
    service.request_feedback("doc", "request-2");
    let model = Arc::new(ScriptedModel::new(vec![text("First."), text("Second.")]));
    let (agent, chat, _) = linked_chat(&service, directory.path(), model.clone()).await;
    eventually(async || service.finished().len() == 2).await;
    eventually(async || {
        let snapshot = agent.snapshot(chat).await.unwrap();
        let history = serde_json::to_string(&snapshot.messages).unwrap();
        history.contains("feedback request-1") && history.contains("feedback request-2")
    })
    .await;
    assert!(service.releases().is_empty());
    assert!(service.tool_calls().is_empty());
    agent.shutdown().await;
}

#[tokio::test]
async fn a_normal_run_without_a_comment_keeps_the_feedback_and_link() {
    let service = ArtifactService::start().await;
    let directory = tempfile::tempdir().unwrap();
    service.request_feedback("doc", "request-1");
    let (agent, chat, _loop_id) = linked_chat(
        &service,
        directory.path(),
        Arc::new(ScriptedModel::new(vec![text("I looked at it.")])),
    )
    .await;
    eventually(async || !service.is_pending("request-1")).await;
    wait_for_loop_state(&agent, chat, LoopState::Idle).await;
    assert!(service.releases().is_empty());
    assert!(!agent.snapshot(chat).await.unwrap().chat_loop.event_sources[0].resume_required);
    agent.shutdown().await;
}

struct BlockingModel {
    started: tokio::sync::Notify,
    turns: AtomicUsize,
    requests: Mutex<Vec<ModelRequest>>,
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
        request: ModelRequest,
        _: tokio::sync::mpsc::UnboundedSender<ModelDelta>,
        cancel: CancellationToken,
    ) -> Result<ModelOutput, ModelError> {
        self.requests.lock().unwrap().push(request);
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
async fn esc_then_new_input_keeps_prior_feedback_and_receives_later_feedback_without_replay() {
    let service = ArtifactService::start().await;
    let directory = tempfile::tempdir().unwrap();
    service.request_feedback("doc", "request-1");
    let model = Arc::new(BlockingModel {
        started: tokio::sync::Notify::new(),
        turns: AtomicUsize::new(0),
        requests: Mutex::new(Vec::new()),
    });
    let (agent, chat, _loop_id) = linked_chat(&service, directory.path(), model.clone()).await;
    tokio::time::timeout(WAIT, model.started.notified())
        .await
        .unwrap();
    agent.interrupt(chat).await.unwrap();
    wait_for_loop_state(&agent, chat, LoopState::Interrupted).await;
    assert!(!agent.snapshot(chat).await.unwrap().chat_loop.event_sources[0].resume_required);
    assert!(!service.is_pending("request-1"));
    agent
        .submit(chat, "Please keep going".into())
        .await
        .unwrap();
    eventually(async || model.turns.load(Ordering::SeqCst) >= 2).await;
    let messages = serde_json::to_string(&model.requests.lock().unwrap()[1].messages).unwrap();
    assert!(messages.contains("feedback request-1") && messages.contains("Please keep going"));
    service.request_feedback("doc", "request-2");
    eventually(async || service.finished().len() == 2).await;
    eventually(async || model.turns.load(Ordering::SeqCst) >= 3).await;
    let claim_calls = service
        .requests()
        .iter()
        .filter(|request| {
            request["path"]
                .as_str()
                .is_some_and(|path| path.contains("/claim"))
        })
        .count();
    assert_eq!(claim_calls, 2);
    assert!(service.releases().is_empty());
    agent.shutdown().await;
}

#[tokio::test]
async fn stopping_pauses_the_link_without_releasing_received_input() {
    let service = ArtifactService::start().await;
    let directory = tempfile::tempdir().unwrap();
    service.request_feedback("doc", "request-1");
    let model = Arc::new(BlockingModel {
        started: tokio::sync::Notify::new(),
        turns: AtomicUsize::new(0),
        requests: Mutex::new(Vec::new()),
    });
    let (agent, chat, _loop_id) = linked_chat(&service, directory.path(), model.clone()).await;
    tokio::time::timeout(WAIT, model.started.notified())
        .await
        .unwrap();
    agent.stop(chat).await.unwrap();
    assert!(
        service.link("doc").is_some(),
        "a paused Loop keeps its link"
    );
    let chat_loop = agent.snapshot(chat).await.unwrap().chat_loop;
    assert_eq!(
        chat_loop.receiving,
        tickloop_agent_protocol::Receiving::Paused {
            reason: tickloop_agent_protocol::PauseReason::Stopped
        }
    );
    assert!(
        !chat_loop.event_sources[0].resume_required,
        "pausing leaves the link itself available"
    );
    assert!(!service.is_pending("request-1"));
    assert!(service.releases().is_empty());
    agent.shutdown().await;
}

#[tokio::test]
async fn send_now_keeps_received_feedback_and_a_busy_loop_accepts_new_feedback() {
    let service = ArtifactService::start().await;
    let directory = tempfile::tempdir().unwrap();
    service.request_feedback("doc", "first-feedback");
    let model = Arc::new(BlockingModel {
        started: tokio::sync::Notify::new(),
        turns: AtomicUsize::new(0),
        requests: Mutex::new(Vec::new()),
    });
    let (agent, chat, _loop_id) = linked_chat(&service, directory.path(), model.clone()).await;
    tokio::time::timeout(WAIT, model.started.notified())
        .await
        .unwrap();
    service.request_feedback("doc", "busy-feedback");
    eventually(async || service.finished().len() == 2).await;
    assert_eq!(model.turns.load(Ordering::SeqCst), 1);
    let queued = agent
        .submit(chat, "Use the shorter title".into())
        .await
        .unwrap();
    agent.send_now(chat, queued.run_id).await.unwrap();
    eventually(async || model.turns.load(Ordering::SeqCst) >= 2).await;
    eventually(async || {
        let messages =
            serde_json::to_string(&agent.snapshot(chat).await.unwrap().messages).unwrap();
        messages.contains("first-feedback")
            && messages.contains("busy-feedback")
            && messages.contains("Use the shorter title")
    })
    .await;
    assert!(!agent.snapshot(chat).await.unwrap().chat_loop.event_sources[0].resume_required);
    assert!(service.releases().is_empty());
    agent.shutdown().await;
}

#[tokio::test]
async fn feedback_account_provenance_survives_restart_and_stop_works_after_account_change() {
    let service = ArtifactService::start().await;
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("state");
    let account = service.login(directory.path()).await;
    let blocked = Arc::new(BlockingModel {
        started: tokio::sync::Notify::new(),
        turns: AtomicUsize::new(0),
        requests: Mutex::new(Vec::new()),
    });
    let agent = Agent::open_embedded(root.clone(), config(), blocked.clone()).unwrap();
    agent
        .install_host_tools(Arc::new(McpHostToolProvider::new(account, None)))
        .unwrap();
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    agent
        .set_execution_mode(chat, ExecutionMode::FullAccess)
        .await
        .unwrap();
    service.request_feedback("doc", "private-feedback");
    agent
        .link_artifact(
            chat,
            &artifact_tick(&service, "doc"),
            tickloop_agent_protocol::ArtifactFeedbackHistory::All,
        )
        .await
        .unwrap();
    if tokio::time::timeout(WAIT, blocked.started.notified())
        .await
        .is_err()
    {
        panic!(
            "Feedback did not start: {:?}; HTTP {:?}",
            agent.snapshot(chat).await.unwrap(),
            service.requests()
        );
    }
    agent.interrupt(chat).await.unwrap();
    wait_for_loop_state(&agent, chat, LoopState::Interrupted).await;
    agent.shutdown().await;
    drop(agent);
    service.control.other_account.store(true, Ordering::SeqCst);
    let other = service.login(directory.path()).await;
    let model = Arc::new(ScriptedModel::new(vec![text(
        "Should not see the other account's input.",
    )]));
    let restored = Agent::open_embedded(root, config(), model.clone()).unwrap();
    restored
        .install_host_tools(Arc::new(McpHostToolProvider::new(other, None)))
        .unwrap();
    restored.resume_chat(chat).await.unwrap();
    restored
        .set_execution_mode(chat, ExecutionMode::FullAccess)
        .await
        .unwrap();
    restored.submit(chat, "Continue".into()).await.unwrap();
    wait_for_loop_state(&restored, chat, LoopState::Failed).await;
    assert!(model.requests.lock().unwrap().is_empty());
    restored.stop(chat).await.unwrap();
    let chat_loop = restored.snapshot(chat).await.unwrap().chat_loop;
    assert_eq!(chat_loop.state, LoopState::Failed);
    assert!(
        chat_loop
            .event_sources
            .iter()
            .all(|source| source.resume_required)
    );
    restored.shutdown().await;
}

#[tokio::test]
async fn folderless_feedback_rejects_an_account_change_before_delivery_or_continuation() {
    use tickloop_agent_runtime::{
        HarnessOptions, HarnessSeams, ToolPolicy, TurnRequest, WaitHosting,
    };
    use tickloop_agent_test_support::harness::ChosenModels;

    for switch_before_delivery in [true, false] {
        let service = ArtifactService::start().await;
        let directory = tempfile::tempdir().unwrap();
        let client = service.login(directory.path()).await;
        let model = Arc::new(ScriptedModel::new(vec![text("received")]));
        let agent = Agent::with_seams(
            HarnessSeams {
                transcripts: None,
                environment: None,
                models: ChosenModels::new(model.clone()),
            },
            HarnessOptions {
                host: tickloop_agent_protocol::AgentHostOptions {
                    prompt_policy: tickloop_agent_protocol::PromptPolicy::DenyOnPrompt,
                    ..tickloop_agent_protocol::AgentHostOptions::product_defaults()
                },
                tools: ToolPolicy {
                    artifacts: true,
                    ..ToolPolicy::web_fetch_only()
                },
                waits: WaitHosting::Durable,
                ..HarnessOptions::local(config())
            },
        )
        .unwrap();
        agent
            .install_host_tools(Arc::new(McpHostToolProvider::new(client, None)))
            .unwrap();
        let chat = agent.create_chat_without_folder().await.unwrap();
        agent
            .link_artifact(
                chat,
                &artifact_tick(&service, "doc"),
                tickloop_agent_protocol::ArtifactFeedbackHistory::All,
            )
            .await
            .unwrap();
        let goal = agent.snapshot(chat).await.unwrap().chat_loop.clone();
        let source = &goal.event_sources[0];
        let delivery = tickloop_agent_runtime::ExternalDelivery::from_claim(
            &source.origin,
            &source.account_id,
            &source.artifact_id,
            &tickloop_agent_artifacts::ClaimedFeedback {
                delivery_id: uuid::Uuid::new_v4().to_string(),
                feedback_id: "private-feedback".into(),
                claim_id: "claim".into(),
                fencing_token: "secret-fence".into(),
                lease_expires_at: "2026-10-20T00:00:00Z".into(),
                input: json!({"messages": [{"text": "private input"}]}),
            },
        )
        .unwrap();
        if !switch_before_delivery {
            let turn = agent
                .advance(chat, TurnRequest::deliver(delivery.clone()))
                .await
                .unwrap();
            assert!(matches!(
                turn.outcome().await.unwrap(),
                tickloop_agent_runtime::TurnOutcome::Completed { .. }
            ));
        }
        let requests = model.requests.lock().unwrap().len();
        service.control.other_account.store(true, Ordering::SeqCst);
        service.login(directory.path()).await;
        if switch_before_delivery {
            agent
                .advance(chat, TurnRequest::deliver(delivery))
                .await
                .unwrap();
        } else {
            agent.submit(chat, "Continue".into()).await.unwrap();
        }
        wait_for_loop_state(&agent, chat, LoopState::Failed).await;
        assert_eq!(model.requests.lock().unwrap().len(), requests);
        agent.shutdown().await;
    }
}

/// After a restart the Loop pauses receiving: linking the artifact again is no consent to
/// continue, and only the user's next input delivers the request that waited.
#[tokio::test]
async fn relinking_after_a_restart_waits_for_continue_before_claiming_pending_requests() {
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
    agent.submit(chat, "Get ready".into()).await.unwrap();
    wait_for_completion(&agent, chat).await;
    agent
        .link_artifact(
            chat,
            &artifact_tick(&service, "doc"),
            tickloop_agent_protocol::ArtifactFeedbackHistory::All,
        )
        .await
        .unwrap();
    let first_binding = service.link("doc").unwrap().binding_id;
    agent.shutdown().await;
    drop(agent);

    service.request_feedback("doc", "request-1");
    let model = Arc::new(ScriptedModel::new(vec![
        text("Continuing."),
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
    let goal = &snapshot.chat_loop;
    assert_eq!(goal.state, LoopState::Idle);
    assert_eq!(
        goal.receiving,
        tickloop_agent_protocol::Receiving::Paused {
            reason: tickloop_agent_protocol::PauseReason::HistoryLoaded
        }
    );
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert!(
        service.is_pending("request-1"),
        "history loading never listens"
    );
    restored
        .link_artifact(
            chat,
            &artifact_tick(&service, "doc"),
            tickloop_agent_protocol::ArtifactFeedbackHistory::All,
        )
        .await
        .unwrap();
    let relinked = restored.snapshot(chat).await.unwrap();
    assert_eq!(relinked.chat_loop.id, goal.id);
    assert!(!relinked.chat_loop.receiving.is_receiving());
    let renewed = service.link("doc").unwrap();
    assert_ne!(renewed.binding_id, first_binding);
    assert_eq!(renewed.epoch, 2);
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert!(
        service.is_pending("request-1"),
        "a new link of a paused Loop receives nothing yet"
    );
    restored.submit(chat, "Continue".into()).await.unwrap();
    eventually(async || service.finished() == [("request-1".into(), "received".into())]).await;
    let binding = &restored
        .snapshot(chat)
        .await
        .unwrap()
        .chat_loop
        .event_sources[0];
    assert_eq!(binding.binding_id.to_string(), renewed.binding_id);
    assert_eq!(binding.epoch, 2);
    restored.shutdown().await;
}

/// A request received while its Loop was busy waits in the local inbox across a restart: the
/// next Continue delivers it, it runs once, and the request that was running is not repeated.
#[tokio::test]
async fn a_received_request_still_queued_at_a_restart_runs_once_after_continue() {
    let service = ArtifactService::start().await;
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("state");
    service.login(directory.path()).await;
    service.request_feedback("doc", "running-request");
    let blocked = Arc::new(BlockingModel {
        started: tokio::sync::Notify::new(),
        turns: AtomicUsize::new(0),
        requests: Mutex::new(Vec::new()),
    });
    let agent = Agent::open_embedded(root.clone(), config(), blocked.clone()).unwrap();
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
    agent
        .link_artifact(
            chat,
            &artifact_tick(&service, "doc"),
            tickloop_agent_protocol::ArtifactFeedbackHistory::All,
        )
        .await
        .unwrap();
    tokio::time::timeout(WAIT, blocked.started.notified())
        .await
        .unwrap();
    service.request_feedback("doc", "queued-request");
    eventually(async || service.finished().len() == 2).await;
    agent.shutdown().await;
    drop(agent);

    let model = Arc::new(ScriptedModel::new(vec![
        text("Continued."),
        text("Handled the queued request."),
    ]));
    let restored = Agent::open_embedded(root, config(), model.clone()).unwrap();
    restored
        .install_host_tools(Arc::new(McpHostToolProvider::new(
            service.client(directory.path()),
            None,
        )))
        .unwrap();
    restored.resume_chat(chat).await.unwrap();
    restored
        .set_execution_mode(chat, ExecutionMode::FullAccess)
        .await
        .unwrap();
    restored.submit(chat, "Continue".into()).await.unwrap();
    eventually(async || model.requests.lock().unwrap().len() == 2).await;
    wait_for_loop_state(&restored, chat, LoopState::Idle).await;
    let prompts = |feedback: &str| {
        let heading = format!("@tickloop feedback {feedback} on artifact doc");
        model.requests.lock().unwrap()[1]
            .messages
            .iter()
            .flat_map(|message| &message.content)
            .filter(
                |content| matches!(content, Content::Text { text } if text.starts_with(&heading)),
            )
            .count()
    };
    assert_eq!(prompts("running-request"), 1);
    assert_eq!(prompts("queued-request"), 1);
    assert_eq!(model.requests.lock().unwrap().len(), 2);
    assert_eq!(service.finished().len(), 2);
    restored.shutdown().await;
}

#[tokio::test]
async fn a_copied_session_links_its_own_receiver_without_taking_the_original_agents_feedback() {
    let service = ArtifactService::start().await;
    let directory = tempfile::tempdir().unwrap();
    let account = service.login(directory.path()).await;
    let model = Arc::new(ScriptedModel::new(vec![
        text("Ready."),
        text("Copy ready."),
        text("Original received."),
        text("Received."),
        text("Received."),
    ]));
    let agent = Agent::open_embedded(directory.path().join("state"), config(), model).unwrap();
    agent
        .install_host_tools(Arc::new(McpHostToolProvider::new(account, None)))
        .unwrap();
    let original = agent.create_chat(directory.path().into()).await.unwrap();
    agent.submit(original, "Prepare".into()).await.unwrap();
    wait_for_completion(&agent, original).await;
    agent
        .link_artifact(
            original,
            &artifact_tick(&service, "doc"),
            tickloop_agent_protocol::ArtifactFeedbackHistory::NewOnly,
        )
        .await
        .unwrap();
    let copied = agent.resume_as_new_chat(original).await.unwrap();
    assert!(copied.chat_loop.event_sources.is_empty());
    agent
        .submit(copied.id, "Continue the copied context".into())
        .await
        .unwrap();
    wait_for_completion(&agent, copied.id).await;
    service.request_feedback("doc", "original-receiver");
    eventually(async || {
        service
            .finished()
            .iter()
            .any(|(id, _)| id == "original-receiver")
    })
    .await;
    eventually(async || {
        serde_json::to_string(&agent.snapshot(original).await.unwrap().messages)
            .unwrap()
            .contains("original-receiver")
    })
    .await;
    assert!(
        !serde_json::to_string(&agent.snapshot(copied.id).await.unwrap().messages)
            .unwrap()
            .contains("original-receiver")
    );
    agent
        .link_artifact(
            copied.id,
            &artifact_tick(&service, "doc"),
            tickloop_agent_protocol::ArtifactFeedbackHistory::NewOnly,
        )
        .await
        .unwrap();
    service.request_feedback("doc", "independent-receivers");
    eventually(async || {
        service
            .finished()
            .iter()
            .filter(|(id, _)| id == "independent-receivers")
            .count()
            == 2
    })
    .await;
    for chat in [original, copied.id] {
        eventually(async || {
            serde_json::to_string(&agent.snapshot(chat).await.unwrap().messages)
                .unwrap()
                .contains("independent-receivers")
        })
        .await;
    }
    assert!(
        service
            .lifecycle_requests("DELETE /artifact/api/v1/agent/links/doc")
            .is_empty()
    );
    agent.shutdown().await;
}

#[tokio::test]
async fn a_link_replaced_elsewhere_pauses_listening_until_continue() {
    let service = ArtifactService::start().await;
    let directory = tempfile::tempdir().unwrap();
    let (agent, chat, _loop_id) = linked_chat(
        &service,
        directory.path(),
        Arc::new(ScriptedModel::new(vec![])),
    )
    .await;
    wait_for_loop_state(&agent, chat, LoopState::Idle).await;
    service.replace_link("doc");
    // The idle watcher checks again within its 15 second interval.
    tokio::time::timeout(Duration::from_secs(25), async {
        loop {
            let snapshot = agent.snapshot(chat).await.unwrap();
            if snapshot.chat_loop.event_sources[0].resume_required {
                break snapshot;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .unwrap();
    assert!(agent.snapshot(chat).await.unwrap().chat_loop.event_sources[0].resume_required);
    agent.shutdown().await;
}

#[tokio::test]
async fn signing_out_pauses_feedback_listening() {
    let service = ArtifactService::start().await;
    let directory = tempfile::tempdir().unwrap();
    let (agent, chat, _loop_id) = linked_chat(
        &service,
        directory.path(),
        Arc::new(ScriptedModel::new(vec![])),
    )
    .await;
    wait_for_loop_state(&agent, chat, LoopState::Idle).await;
    agent.disable_event_sources().await;
    let snapshot = agent.snapshot(chat).await.unwrap();
    assert!(snapshot.chat_loop.event_sources[0].resume_required);
    assert_eq!(snapshot.chat_loop.state, LoopState::Idle);
    agent.shutdown().await;
}

#[tokio::test]
async fn relinking_an_artifact_replaces_its_link_and_another_artifact_joins_the_loop() {
    let service = ArtifactService::start().await;
    let directory = tempfile::tempdir().unwrap();
    let (agent, chat, loop_id) = linked_chat(
        &service,
        directory.path(),
        Arc::new(ScriptedModel::new(vec![text("Second goal.")])),
    )
    .await;
    agent
        .submit(chat, "An unrelated goal".into())
        .await
        .unwrap();
    wait_for_completion(&agent, chat).await;
    agent
        .link_artifact(
            chat,
            &artifact_tick(&service, "doc"),
            tickloop_agent_protocol::ArtifactFeedbackHistory::All,
        )
        .await
        .unwrap();
    let snapshot = agent.snapshot(chat).await.unwrap();
    assert_eq!(snapshot.chat_loop.id, loop_id);
    assert_eq!(snapshot.chat_loop.event_sources.len(), 1);
    assert_eq!(
        snapshot.chat_loop.event_sources[0].binding_id.to_string(),
        service.link("doc").unwrap().binding_id
    );

    agent
        .link_artifact(
            chat,
            &artifact_tick(&service, "other"),
            tickloop_agent_protocol::ArtifactFeedbackHistory::All,
        )
        .await
        .unwrap();
    let snapshot = agent.snapshot(chat).await.unwrap();
    let artifacts = snapshot
        .chat_loop
        .event_sources
        .iter()
        .map(|source| source.artifact_id.as_str())
        .collect::<Vec<_>>();
    assert_eq!(
        artifacts,
        ["doc", "other"],
        "a new link joins the Chat's Loop"
    );
    assert_eq!(snapshot.chat_loop.state, LoopState::Idle);
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
    // Unlink before any feedback is created; only the remaining receiver gets new input.
    agent.submit(chat, "Write two briefs".into()).await.unwrap();
    tokio::time::timeout(WAIT, model.started.notified())
        .await
        .unwrap();
    let doc = artifact_tick(&service, "doc");
    let other = artifact_tick(&service, "other");
    for tick in [&doc, &other] {
        agent.add_artifact_tick(chat, tick.clone()).await.unwrap();
        agent
            .link_artifact(
                chat,
                tick,
                tickloop_agent_protocol::ArtifactFeedbackHistory::All,
            )
            .await
            .unwrap();
    }
    agent
        .remove_artifact_tick(chat, &doc.instance_id())
        .await
        .unwrap();
    let snapshot = agent.snapshot(chat).await.unwrap();
    assert_eq!(snapshot.artifact_ticks, std::slice::from_ref(&other));
    assert_eq!(
        snapshot
            .chat_loop
            .event_sources
            .iter()
            .map(|source| source.artifact_id.as_str())
            .collect::<Vec<_>>(),
        ["other"]
    );

    service.request_feedback("doc", "request-1");
    service.request_feedback("other", "request-2");
    model.release.notify_one();
    eventually(async || {
        service
            .finished()
            .contains(&("request-2".into(), "received".into()))
    })
    .await;
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert!(service.is_pending("request-1"));
    assert!(
        service
            .lifecycle_requests("POST /artifact/api/v1/agent/feedback/")
            .iter()
            .all(|request| request["body"]["artifact_id"] != "doc"
                || !request["path"].as_str().unwrap().contains("/ack "))
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
            .any(|conversation| conversation.contains("@tickloop feedback request-2"))
    );
    assert!(
        conversations
            .iter()
            .all(|conversation| !conversation.contains("request-1")),
        "an unlinked artifact's request never reaches the Loop"
    );

    wait_for_loop_state(&agent, chat, LoopState::Idle).await;
    agent
        .remove_artifact_tick(chat, &other.instance_id())
        .await
        .unwrap();
    let goal = agent.snapshot(chat).await.unwrap().chat_loop.clone();
    assert!(goal.event_sources.is_empty());
    assert_eq!(
        goal.state,
        LoopState::Idle,
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
    agent.submit(chat, "Mock up a board".into()).await.unwrap();
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
    assert!(snapshot.chat_loop.event_sources.is_empty());
    eventually(async || service.link("doc").is_none()).await;
    assert!(
        service
            .lifecycle_requests("PUT /artifact/api/v1/agent/links/")
            .is_empty()
    );
    assert_eq!(service.tool_calls().len(), 1);
    agent.shutdown().await;
}

#[tokio::test]
async fn an_artifact_result_reaches_the_model_once_and_a_page_read_stays_inline() {
    let service = ArtifactService::start().await;
    let directory = tempfile::tempdir().unwrap();
    let section = "<section class=\"step\"><h2 title=\"step\">Step</h2><p>Sandbox \"policy\" text.</p></section>\n";
    let source = format!(
        "<!doctype html><html><body>{}</body></html>",
        section.repeat(24_000 / section.len())
    );
    assert!(source.len() > 20_000);
    let body = json!({"artifact_id":CREATED_ARTIFACT,"title":"Fixture","format":"page","url":"https://example.invalid/artifact/page","access_role":"owner","revision":7,"source":source});
    *service.control.read_result.lock().unwrap() = Some(
        json!({"content":[{"type":"text","text":body.to_string()}],"structuredContent":body,"isError":false}),
    );
    let model = Arc::new(ScriptedModel::new(vec![
        call(
            "page",
            "ArtifactRead",
            json!({"action":"read","artifact_id":CREATED_ARTIFACT}),
        ),
        text("Read the page."),
        call(
            "plain",
            "ArtifactRead",
            json!({"action":"read","artifact_id":CREATED_ARTIFACT}),
        ),
        text("Read the plain result."),
    ]));
    let account = service.login(directory.path()).await;
    let agent =
        Agent::open_embedded(directory.path().join("state"), config(), model.clone()).unwrap();
    agent
        .install_host_tools(Arc::new(McpHostToolProvider::new(account, None)))
        .unwrap();
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    submit_and_wait(&agent, chat, "Read the page").await;
    *service.control.read_result.lock().unwrap() = Some(json!({
        "content":[{"type":"text","text":"first line"},{"type":"text","text":"second line"}],
        "isError":false
    }));
    submit_and_wait(&agent, chat, "Read it again").await;

    {
        let requests = model.requests.lock().unwrap();
        let page = tool_result_text(&requests[1], "page");
        assert_eq!(serde_json::from_str::<Value>(&page).unwrap(), body);
        assert_eq!(
            page.matches("Sandbox").count(),
            source.matches("Sandbox").count()
        );
        assert!(!page.contains("structuredContent"));
        assert!(!page.contains("<persisted-output>"));
        assert_eq!(
            tool_result_text(&requests[3], "plain"),
            "first line\nsecond line"
        );
    }
    agent.shutdown().await;
}

#[tokio::test]
async fn reading_back_an_unchanged_page_this_chat_wrote_returns_a_note_instead_of_its_source() {
    let service = ArtifactService::start().await;
    let directory = tempfile::tempdir().unwrap();
    let source = "<!doctype html><html><head><title>Fixture</title></head><body><p>Own words</p></body></html>";
    let model = Arc::new(ScriptedModel::new(vec![
        call(
            "create",
            "ArtifactEdit",
            json!({"action":"create","title":"Fixture","format":"page","content":source}),
        ),
        text("Created."),
        call(
            "own",
            "ArtifactRead",
            json!({"action":"read","artifact_id":CREATED_ARTIFACT}),
        ),
        text("Read my page."),
        call(
            "changed",
            "ArtifactRead",
            json!({"action":"read","artifact_id":CREATED_ARTIFACT}),
        ),
        text("Read the edited page."),
    ]));
    let agent = signed_in_agent(&service, directory.path(), model.clone()).await;
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    let turn = async |prompt: &str, requests: usize| {
        agent.submit(chat, prompt.into()).await.unwrap();
        eventually(async || model.requests.lock().unwrap().len() >= requests).await;
    };
    turn("Make a page", 2).await;
    let page = |revision: u64, source: &str| {
        let body = json!({"artifact_id":CREATED_ARTIFACT,"title":"Fixture","format":"page","url":"https://example.invalid/artifact/page","access_role":"owner","revision":revision,"source":source});
        json!({"content":[{"type":"text","text":body.to_string()}],"structuredContent":body,"isError":false})
    };
    *service.control.read_result.lock().unwrap() = Some(page(0, source));
    turn("Read it back", 4).await;
    let edited = "<!doctype html><html><head><title>Fixture</title></head><body><p>Someone else's words</p></body></html>";
    *service.control.read_result.lock().unwrap() = Some(page(1, edited));
    turn("Read it again", 6).await;
    {
        let requests = model.requests.lock().unwrap();
        let own = tool_result_text(&requests[3], "own");
        assert!(!own.contains("Own words"), "{own}");
        let own: Value = serde_json::from_str(&own).unwrap();
        assert_eq!(own["revision"], 0);
        assert!(
            own["source_unchanged"]
                .as_str()
                .unwrap()
                .contains("edit from it instead of reading it again"),
            "{own}"
        );
        let changed = tool_result_text(&requests[5], "changed");
        assert!(changed.contains("Someone else's words"), "{changed}");
    }
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
    let mut compact_body = body.clone();
    compact_body["artifact_id"] = json!(short_id);
    *service.control.read_result.lock().unwrap() = Some(
        json!({"content":[{"type":"text","text":compact_body.to_string()}],"structuredContent":compact_body,"isError":false}),
    );
    submit_and_wait(&agent, chat, "Read the next page and reread the document").await;
    let snapshot = agent.snapshot(chat).await.unwrap();
    assert_eq!(snapshot.artifact_ticks, first.artifact_ticks);
    assert!(snapshot.chat_loop.event_sources.is_empty());
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
        ("slides", "edit", "slides", "editor"),
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
        assert!(snapshot.chat_loop.event_sources.is_empty());
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
    } else if format == "slides" {
        body["blocks"] = json!([{"id":"cover","hash":"c1","text":"<section id=\"cover\" style=\"background:#ffffff\"><h1>Fixture</h1></section>"}]);
        body["next_cursor"] = Value::Null;
        body["assets"] = json!([]);
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
    assert!(snapshot.chat_loop.event_sources.is_empty());
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
    agent.submit(chat, "Write a brief".into()).await.unwrap();
    eventually(async || agent.snapshot(chat).await.unwrap().chat_loop.state == LoopState::Idle)
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
    let goal = snapshot.chat_loop;
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
    agent.submit(chat, "Write a brief".into()).await.unwrap();
    wait_for_completion(&agent, chat).await;
    let snapshot = agent.snapshot(chat).await.unwrap();
    assert_eq!(snapshot.artifact_ticks.len(), 1);
    assert_eq!(snapshot.artifact_ticks[0].format, "html");
    assert!(snapshot.chat_loop.event_sources.is_empty());
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
    wait_for_loop_state(&agent, chat, LoopState::Idle).await;
    service.control.other_account.store(true, Ordering::SeqCst);
    service.login(directory.path()).await;
    tokio::time::timeout(Duration::from_secs(25), async {
        while !agent.snapshot(chat).await.unwrap().chat_loop.event_sources[0].resume_required {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("the previous account's link pauses");
    agent
        .link_artifact(
            chat,
            &artifact_tick(&service, "doc"),
            tickloop_agent_protocol::ArtifactFeedbackHistory::All,
        )
        .await
        .unwrap();
    let snapshot = agent.snapshot(chat).await.unwrap();
    assert_eq!(snapshot.chat_loop.id, loop_id);
    let sources = &snapshot.chat_loop.event_sources;
    assert_eq!(
        sources.len(),
        1,
        "the new account's link replaces the old one"
    );
    assert_eq!(sources[0].account_id, "user-two");
    assert!(!sources[0].resume_required);
    assert_eq!(snapshot.chat_loop.state, LoopState::Idle);
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
    agent.submit(chat, prompt.into()).await.unwrap();
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
                "host tools available in this conversation now: ArtifactComment, ArtifactEdit, ArtifactRead, ArtifactUpload."
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
    let tool_notes = snapshot
        .host_notes
        .iter()
        .filter(|(_, note)| note.announced_host_tools().is_some())
        .collect::<Vec<_>>();
    assert_eq!(tool_notes.len(), 1);
    let (index, announced) = tool_notes[0];
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
    assert_eq!(notices.len(), 2, "{notices:?}");
    assert!(
        notices[1].contains("tickloop-artifacts")
            && notices[1].contains("Their instructions above no longer apply"),
        "{}",
        notices[1]
    );
    assert!(
        notices[0].contains("Now available: ArtifactLogin.")
            && notices[0].contains(
                "No longer available: ArtifactComment, ArtifactEdit, ArtifactRead, ArtifactUpload."
            ),
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

/// Shutdown ends a sign-in still waiting for approval: approving it afterwards reaches no poll,
/// and no credential is saved, though the Agent is still held.
#[tokio::test]
async fn shutdown_ends_a_pending_artifact_sign_in_before_it_can_save_a_credential() {
    const TOKEN_POLL: &str = "POST /account/token ";
    let service = ArtifactService::start().await;
    let directory = tempfile::tempdir().unwrap();
    service
        .control
        .hold_device_approval
        .store(true, Ordering::SeqCst);
    let model = Arc::new(ScriptedModel::new(vec![
        call("login", "ArtifactLogin", json!({})),
        text("Open the link and confirm the code."),
    ]));
    let agent = Agent::new_embedded(config(), model);
    agent
        .install_host_tools(Arc::new(McpHostToolProvider::new(
            service.client(directory.path()),
            None,
        )))
        .unwrap();
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    submit_and_wait(&agent, chat, "Make me a design").await;
    eventually(async || !service.lifecycle_requests(TOKEN_POLL).is_empty()).await;

    agent.shutdown().await;
    let polls = service.lifecycle_requests(TOKEN_POLL).len();
    service
        .control
        .hold_device_approval
        .store(false, Ordering::SeqCst);
    // The fixture asks for a poll every second.
    tokio::time::sleep(Duration::from_millis(2500)).await;
    assert_eq!(service.lifecycle_requests(TOKEN_POLL).len(), polls);
    assert!(matches!(
        service.client(directory.path()).credential_snapshot(),
        Ok(None)
    ));
    drop(agent);
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
            started.contains(&format!("{}/account/device?code=TEST", service.origin))
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
        notices[0].contains(
            "Now available: ArtifactComment, ArtifactEdit, ArtifactRead, ArtifactUpload."
        ) && notices[0].contains("No longer available: ArtifactLogin."),
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
    agent.submit(chat, "Write a doc".into()).await.unwrap();
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
        .submit(chat, "Delegate the draft".into())
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
        text(&"older material ".repeat(23_000)),
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
    agent.set_context_window(chat, 200_000).await.unwrap();
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
            "host tools available in this conversation now: ArtifactComment, ArtifactEdit, ArtifactRead, ArtifactUpload."
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
                "ReadMcpResourceTool",
                json!({"server":"tickloop-artifacts","uri":"skill://tickloop-artifacts/artifact-design/references/guide.md"}),
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
                assert!(!guidance.contains("Reference fixture: preserve data units"));
            }
        }
        agent.shutdown().await;
    }
}

#[tokio::test]
async fn remote_resource_requested_after_compaction_returns_complete_content_without_changing_skill()
 {
    let service = ArtifactService::start().await;
    service.control.skills_enabled.store(true, Ordering::SeqCst);
    service
        .control
        .skill_reference_bytes
        .store(30_000, Ordering::SeqCst);
    let directory = tempfile::tempdir().unwrap();
    let model = Arc::new(ScriptedModel::new(vec![
        call(
            "load",
            "Skill",
            json!({"skill":"tickloop-artifacts:artifact-design","args":"preserve these arguments"}),
        ),
        call(
            "reference",
            "ReadMcpResourceTool",
            json!({"server":"tickloop-artifacts","uri":"skill://tickloop-artifacts/artifact-design/references/guide.md"}),
        ),
        text("done"),
        text("A compact summary"),
        call(
            "reload",
            "ReadMcpResourceTool",
            json!({"server":"tickloop-artifacts","uri":"skill://tickloop-artifacts/artifact-design/references/guide.md"}),
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
        assert!(!guidance(5).contains("REFERENCE_END"));
        assert!(guidance(5).contains("preserve these arguments"));
        assert!(
            serde_json::to_string(&requests[5].messages)
                .unwrap()
                .contains("REFERENCE_END")
        );
        assert_eq!(guidance(4), guidance(5));
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
                text(&"older material ".repeat(23_000)),
                text("A compact summary"),
                text("continued"),
            ]),
            started: tokio::sync::Notify::new(),
            release: tokio::sync::Notify::new(),
        });
        let agent = signed_in_agent(&service, directory.path(), model.clone()).await;
        let chat = agent.create_chat(directory.path().into()).await.unwrap();
        submit_and_wait(&agent, chat, "Build a page").await;
        agent.set_context_window(chat, 200_000).await.unwrap();
        agent
            .submit(chat, "Continue with the page".into())
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
            text(&"older material ".repeat(23_000)),
            text("A compact summary"),
            text("must not run"),
        ]),
        started: tokio::sync::Notify::new(),
        release: tokio::sync::Notify::new(),
    });
    let agent = signed_in_agent(&service, directory.path(), model.clone()).await;
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    submit_and_wait(&agent, chat, "Build a page").await;
    agent.set_context_window(chat, 200_000).await.unwrap();
    agent
        .submit(chat, "Continue with the page".into())
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
        wait_for_loop_state(&agent, chat, LoopState::Interrupted),
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

#[tokio::test]
async fn created_slides_join_monitor_as_slides_and_link_to_the_creating_loop() {
    let service = ArtifactService::start().await;
    let directory = tempfile::tempdir().unwrap();
    let model = Arc::new(ScriptedModel::new(vec![
        call(
            "create",
            "ArtifactEdit",
            json!({"action":"create","title":"Launch","format":"slides","content":"<section id=\"cover\" style=\"background:#ffffff\"><h1>Launch</h1></section>"}),
        ),
        text("Created."),
    ]));
    let agent = signed_in_agent(&service, directory.path(), model).await;
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    let mut events = agent.subscribe();
    agent
        .submit(chat, "做个 PPT 介绍这次发布".into())
        .await
        .unwrap();
    eventually(async || agent.snapshot(chat).await.unwrap().chat_loop.state == LoopState::Idle)
        .await;
    while let Ok(event) = events.try_recv() {
        assert!(
            !matches!(event.kind, EventKind::PermissionRequested { .. }),
            "a new private Slides takes the first-party creation fast path"
        );
    }
    let snapshot = agent.snapshot(chat).await.unwrap();
    assert_eq!(snapshot.artifact_ticks.len(), 1);
    assert_eq!(snapshot.artifact_ticks[0].artifact_id, CREATED_ARTIFACT);
    assert_eq!(snapshot.artifact_ticks[0].format, "slides");
    let goal = snapshot.chat_loop;
    assert_eq!(goal.event_sources[0].artifact_id, CREATED_ARTIFACT);
    assert_eq!(
        service.link(CREATED_ARTIFACT).unwrap().loop_id,
        goal.id.to_string()
    );
    agent.shutdown().await;
}

fn png(label: &[u8]) -> Vec<u8> {
    [&[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a][..], label].concat()
}

/// The fixture artifact is a Slides published to `audience`.
fn slides_published_to(service: &ArtifactService, audience: &str) {
    *service.control.metadata_format.lock().unwrap() = Some("slides".into());
    *service.control.publication.lock().unwrap() =
        Some(json!({"audience":audience,"membership_revision":1}));
}

fn upload_call(id: &str, path: &std::path::Path) -> Vec<Content> {
    call(
        id,
        "ArtifactUpload",
        json!({"artifact_id":"deck","file_path":path}),
    )
}

fn sha256(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    format!("{:x}", Sha256::digest(bytes))
}

#[tokio::test]
async fn an_upload_from_the_workspace_sends_the_read_bytes_once_and_returns_only_the_reference() {
    let service = ArtifactService::start().await;
    slides_published_to(&service, "private");
    let directory = tempfile::tempdir().unwrap();
    let image = png(b"workspace chart pixels");
    let path = directory.path().join("chart.png");
    std::fs::write(&path, &image).unwrap();
    let model = Arc::new(ScriptedModel::new(vec![
        upload_call("upload", &path),
        text("Uploaded."),
    ]));
    let agent = signed_in_agent(&service, directory.path(), model.clone()).await;
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    agent
        .set_execution_mode(chat, ExecutionMode::AcceptEdits)
        .await
        .unwrap();
    let mut events = agent.subscribe();
    submit_and_wait(&agent, chat, "Put the chart on the Slides").await;
    while let Ok(event) = events.try_recv() {
        assert!(
            !matches!(event.kind, EventKind::PermissionRequested { .. }),
            "a workspace image for private Slides is a first-party private write"
        );
    }
    let uploads = service.uploads();
    assert_eq!(uploads.len(), 1);
    assert_eq!(uploads[0].artifact_id, "deck");
    assert_eq!(uploads[0].content_type, "image/png");
    assert_eq!(uploads[0].bytes, image);
    assert!(uuid::Uuid::parse_str(&uploads[0].operation_id).is_ok());
    let result: Value = serde_json::from_str(&tool_result_text(
        &model.requests.lock().unwrap()[1],
        "upload",
    ))
    .unwrap();
    assert_eq!(
        result,
        json!({"asset_id":"asset-1","ref":"asset:asset-1","content_type":"image/png","bytes":image.len(),"width":640,"height":360})
    );
    let snapshot = agent.snapshot(chat).await.unwrap();
    let operation = snapshot
        .operations
        .iter()
        .find(|operation| operation.name == "ArtifactUpload")
        .unwrap();
    assert_eq!(operation.state, OperationState::Completed);
    assert_eq!(
        operation.input,
        json!({"artifact_id":"deck","file_path":path.canonicalize().unwrap(),"content_type":"image/png","bytes":image.len(),"sha256":sha256(&image)})
    );
    let saved = serde_json::to_string(&snapshot).unwrap();
    assert!(
        !saved.contains("workspace chart pixels"),
        "the file's contents never enter the Chat"
    );
    agent.shutdown().await;
}

#[tokio::test]
async fn an_upload_whose_metadata_the_service_strips_reports_the_stored_image() {
    let service = ArtifactService::start().await;
    slides_published_to(&service, "private");
    service
        .control
        .strip_upload_metadata
        .store(true, Ordering::SeqCst);
    let directory = tempfile::tempdir().unwrap();
    let image = png(b"tEXt camera serial and location");
    let path = directory.path().join("photo.png");
    std::fs::write(&path, &image).unwrap();
    let model = Arc::new(ScriptedModel::new(vec![
        upload_call("upload", &path),
        text("Uploaded."),
    ]));
    let agent = signed_in_agent(&service, directory.path(), model.clone()).await;
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    agent
        .set_execution_mode(chat, ExecutionMode::FullAccess)
        .await
        .unwrap();
    submit_and_wait(&agent, chat, "Put the photo on the Slides").await;
    assert_eq!(service.uploads()[0].bytes, image);
    let result: Value = serde_json::from_str(&tool_result_text(
        &model.requests.lock().unwrap()[1],
        "upload",
    ))
    .unwrap();
    assert_eq!(result["ref"], "asset:asset-1");
    assert_eq!(result["bytes"], 8, "the size of the stored, stripped image");
    assert!(
        service
            .lifecycle_requests("GET /artifact/api/v1/artifacts/deck/operations/")
            .is_empty(),
        "the direct receipt confirms the upload"
    );
    let operation = agent
        .snapshot(chat)
        .await
        .unwrap()
        .operations
        .into_iter()
        .find(|operation| operation.name == "ArtifactUpload")
        .unwrap();
    assert_eq!(operation.state, OperationState::Completed);
    agent.shutdown().await;
}

#[tokio::test]
async fn an_upload_from_outside_the_workspace_asks_once_with_the_file_facts_but_not_its_contents() {
    let service = ArtifactService::start().await;
    slides_published_to(&service, "private");
    let directory = tempfile::tempdir().unwrap();
    let elsewhere = tempfile::tempdir().unwrap();
    let image = png(b"holiday photo pixels");
    let path = elsewhere.path().join("photo.png");
    std::fs::write(&path, &image).unwrap();
    let model = Arc::new(ScriptedModel::new(vec![
        upload_call("upload", &path),
        text("Uploaded."),
    ]));
    let agent = signed_in_agent(&service, directory.path(), model.clone()).await;
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    agent
        .set_execution_mode(chat, ExecutionMode::AcceptEdits)
        .await
        .unwrap();
    let mut events = agent.subscribe();
    agent
        .submit(chat, "Add my photo to the Slides".into())
        .await
        .unwrap();
    let permission = next_permission(&mut events).await;
    assert_eq!(permission.name, "ArtifactUpload");
    assert_eq!(
        permission.input,
        json!({"artifact_id":"deck","file_path":path.canonicalize().unwrap(),"content_type":"image/png","bytes":image.len(),"sha256":sha256(&image)})
    );
    assert!(
        service.uploads().is_empty(),
        "nothing is sent before approval"
    );
    agent
        .answer_permission(chat, permission.id, true)
        .await
        .unwrap();
    wait_for_completion(&agent, chat).await;
    assert_eq!(service.uploads().len(), 1);
    assert_eq!(service.uploads()[0].bytes, image);
    let history = agent.approval_history_view(chat).await.unwrap();
    assert!(
        history
            .records
            .iter()
            .any(|record| record.tool == "ArtifactUpload")
    );
    let recorded = serde_json::to_string(&history).unwrap()
        + &serde_json::to_string(&agent.snapshot(chat).await.unwrap()).unwrap();
    assert!(!recorded.contains("holiday photo pixels"));
    agent.shutdown().await;
}

#[tokio::test]
async fn plan_mode_asks_for_a_private_slides_upload_as_it_does_for_artifact_edits() {
    let service = ArtifactService::start().await;
    slides_published_to(&service, "private");
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("logo.png");
    std::fs::write(&path, png(b"logo")).unwrap();
    let model = Arc::new(ScriptedModel::new(vec![
        upload_call("upload", &path),
        text("Uploaded while planning."),
    ]));
    let agent = signed_in_agent(&service, directory.path(), model).await;
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    agent
        .set_execution_mode(chat, ExecutionMode::Plan)
        .await
        .unwrap();
    let mut events = agent.subscribe();
    agent
        .submit(chat, "Plan the Slides with the logo".into())
        .await
        .unwrap();
    let permission = next_permission(&mut events).await;
    assert_eq!(permission.name, "ArtifactUpload");
    agent
        .answer_permission(chat, permission.id, true)
        .await
        .unwrap();
    wait_for_completion(&agent, chat).await;
    assert_eq!(service.uploads().len(), 1);
    agent.shutdown().await;
}

#[tokio::test]
async fn sensitive_paths_non_images_and_non_slides_artifacts_are_refused_before_anything_is_sent() {
    let service = ArtifactService::start().await;
    slides_published_to(&service, "private");
    let directory = tempfile::tempdir().unwrap();
    std::fs::create_dir(directory.path().join("secrets")).unwrap();
    let secret = directory.path().join("secrets/chart.png");
    std::fs::write(&secret, png(b"secret")).unwrap();
    let notes = directory.path().join("notes.txt");
    std::fs::write(&notes, "plain notes").unwrap();
    let logo = directory.path().join("logo.png");
    std::fs::write(&logo, png(b"logo")).unwrap();
    let model = Arc::new(ScriptedModel::new(vec![
        upload_call("secret", &secret),
        upload_call("notes", &notes),
        text("Refused."),
        upload_call("doc", &logo),
        text("Refused again."),
    ]));
    let agent = signed_in_agent(&service, directory.path(), model.clone()).await;
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    agent
        .set_execution_mode(chat, ExecutionMode::FullAccess)
        .await
        .unwrap();
    submit_and_wait(&agent, chat, "Add the files").await;
    *service.control.metadata_format.lock().unwrap() = Some("markdown".into());
    submit_and_wait(&agent, chat, "Add the logo to the report").await;
    {
        let requests = model.requests.lock().unwrap();
        assert!(
            tool_result_text(&requests[2], "secret").contains("credentials or secrets"),
            "{}",
            tool_result_text(&requests[2], "secret")
        );
        assert!(
            tool_result_text(&requests[2], "notes")
                .contains("is not a PNG, JPEG, GIF, WebP or SVG image")
        );
        assert!(
            tool_result_text(&requests[4], "doc")
                .contains("adds images to Slides only; this artifact is a Doc")
        );
    }
    assert!(service.uploads().is_empty());
    assert!(
        service
            .lifecycle_requests("POST /artifact/api/v1/artifacts/")
            .is_empty()
    );
    agent.shutdown().await;
}

#[tokio::test]
async fn a_lost_upload_response_is_settled_by_its_operation_receipt_and_never_sent_again() {
    let service = ArtifactService::start().await;
    slides_published_to(&service, "private");
    service
        .control
        .drop_upload_response
        .store(true, Ordering::SeqCst);
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("chart.png");
    std::fs::write(&path, png(b"chart")).unwrap();
    let model = Arc::new(ScriptedModel::new(vec![
        upload_call("confirmed", &path),
        text("Confirmed."),
        upload_call("unconfirmed", &path),
        text("Unknown."),
    ]));
    let agent = signed_in_agent(&service, directory.path(), model.clone()).await;
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    agent
        .set_execution_mode(chat, ExecutionMode::FullAccess)
        .await
        .unwrap();
    submit_and_wait(&agent, chat, "Add the chart").await;
    let posts = || {
        service
            .lifecycle_requests("POST /artifact/api/v1/artifacts/deck/assets?operation_id=")
            .len()
    };
    let receipts = || {
        service
            .lifecycle_requests("GET /artifact/api/v1/artifacts/deck/operations/")
            .len()
    };
    assert_eq!((posts(), receipts()), (1, 1));
    let confirmed = tool_result_text(&model.requests.lock().unwrap()[1], "confirmed");
    assert!(
        confirmed.contains("\"ref\":\"asset:asset-1\""),
        "{confirmed}"
    );

    service
        .control
        .forget_upload_operations
        .store(true, Ordering::SeqCst);
    submit_and_wait(&agent, chat, "Add it once more").await;
    assert_eq!(
        (posts(), receipts()),
        (2, 2),
        "one send and one receipt each"
    );
    let unconfirmed = tool_result_text(&model.requests.lock().unwrap()[3], "unconfirmed");
    assert!(
        unconfirmed.contains("outcome unknown")
            && unconfirmed.contains("read the Slides with ArtifactRead"),
        "{unconfirmed}"
    );
    let snapshot = agent.snapshot(chat).await.unwrap();
    assert_eq!(
        snapshot
            .operations
            .iter()
            .filter(|operation| operation.name == "ArtifactUpload")
            .map(|operation| operation.state.clone())
            .collect::<Vec<_>>(),
        [OperationState::Completed, OperationState::Unknown]
    );
    agent.shutdown().await;
}

#[tokio::test]
async fn an_admitted_slides_write_still_asks_before_sending_a_file_from_outside_the_workspace() {
    let service = ArtifactService::start().await;
    slides_published_to(&service, "private");
    let directory = tempfile::tempdir().unwrap();
    let elsewhere = tempfile::tempdir().unwrap();
    let inside = directory.path().join("chart.png");
    std::fs::write(&inside, png(b"chart")).unwrap();
    let outside = elsewhere.path().join("photo.png");
    std::fs::write(&outside, png(b"photo")).unwrap();
    let model = Arc::new(ScriptedModel::new(vec![
        upload_call("inside", &inside),
        upload_call("outside", &outside),
        text("Both uploaded."),
    ]));
    let agent = signed_in_agent(&service, directory.path(), model).await;
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    agent
        .set_execution_mode(chat, ExecutionMode::AcceptEdits)
        .await
        .unwrap();
    let mut events = agent.subscribe();
    agent
        .submit(chat, "Add the chart and my photo".into())
        .await
        .unwrap();
    let permission = next_permission(&mut events).await;
    assert_eq!(
        permission.input["file_path"],
        json!(outside.canonicalize().unwrap())
    );
    assert_eq!(
        service.uploads().len(),
        1,
        "the workspace file went first without asking"
    );
    agent
        .answer_permission(chat, permission.id, true)
        .await
        .unwrap();
    wait_for_completion(&agent, chat).await;
    assert_eq!(service.uploads().len(), 2);
    agent.shutdown().await;
}

#[tokio::test]
async fn an_upload_allow_rule_still_asks_before_reading_a_file_from_outside_the_workspace() {
    use tickloop_agent_protocol::Decision;
    let service = ArtifactService::start().await;
    slides_published_to(&service, "private");
    let directory = tempfile::tempdir().unwrap();
    let elsewhere = tempfile::tempdir().unwrap();
    let path = elsewhere.path().join("photo.png");
    std::fs::write(&path, png(b"photo")).unwrap();
    let model = Arc::new(ScriptedModel::new(vec![
        upload_call("upload", &path),
        text("Uploaded."),
    ]));
    let agent = signed_in_agent(&service, directory.path(), model).await;
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    agent
        .set_execution_mode(chat, ExecutionMode::AcceptEdits)
        .await
        .unwrap();
    let mut allow_upload = read_rule(Decision::Allow, None);
    allow_upload.tool = "ArtifactUpload".into();
    agent
        .set_permission_rules(chat, vec![allow_upload])
        .await
        .unwrap();
    let mut events = agent.subscribe();
    agent
        .submit(chat, "Add my photo to the Slides".into())
        .await
        .unwrap();
    let permission = next_permission(&mut events).await;
    assert_eq!(permission.name, "ArtifactUpload");
    assert!(
        service.uploads().is_empty(),
        "nothing is sent before approval"
    );
    agent
        .answer_permission(chat, permission.id, true)
        .await
        .unwrap();
    wait_for_completion(&agent, chat).await;
    assert_eq!(service.uploads().len(), 1);
    agent.shutdown().await;
}

fn read_rule(
    decision: tickloop_agent_protocol::Decision,
    input: Option<Value>,
) -> tickloop_agent_protocol::PermissionRule {
    tickloop_agent_protocol::PermissionRule {
        id: uuid::Uuid::new_v4(),
        decision,
        tool: "Read".into(),
        specifier: None,
        input,
        loop_id: None,
        persistent: false,
    }
}

#[tokio::test]
async fn a_subagent_uploads_only_files_its_own_read_could_take() {
    let service = ArtifactService::start().await;
    slides_published_to(&service, "private");
    let directory = tempfile::tempdir().unwrap();
    let elsewhere = tempfile::tempdir().unwrap();
    let inside = directory.path().join("chart.png");
    std::fs::write(&inside, png(b"workspace chart")).unwrap();
    let outside = elsewhere.path().join("hosts");
    std::fs::write(&outside, "127.0.0.1 localhost").unwrap();
    let model = Arc::new(ScriptedModel::new(vec![
        call(
            "delegate",
            "Subagent",
            json!({"task":"Put the images on the Slides"}),
        ),
        upload_call("inside", &inside),
        upload_call("outside", &outside),
        text("Child uploaded what it could."),
        text("Done."),
    ]));
    let agent = signed_in_agent(&service, directory.path(), model.clone()).await;
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    agent
        .set_execution_mode(chat, ExecutionMode::FullAccess)
        .await
        .unwrap();
    submit_and_wait(&agent, chat, "Delegate the images").await;
    let uploads = service.uploads();
    assert_eq!(uploads.len(), 1, "only the file inside the delegated scope");
    assert_eq!(uploads[0].bytes, png(b"workspace chart"));
    let requests = model.requests.lock().unwrap();
    assert!(tool_result_text(&requests[2], "inside").contains("\"ref\":\"asset:asset-1\""));
    let refused = tool_result_text(&requests[3], "outside");
    assert!(
        refused.contains("outside the delegated resource scope")
            && !refused.contains("is not a PNG"),
        "the file is refused before it is opened: {refused}"
    );
}

#[tokio::test]
async fn read_rules_govern_the_file_an_upload_sends() {
    use tickloop_agent_protocol::Decision;
    let exact_read =
        |path: &std::path::Path| Some(json!({"file_path": path.canonicalize().unwrap()}));
    for (decision, is_exact, prompts, sent) in [
        (Decision::Deny, false, 0, 0),
        (Decision::Deny, true, 0, 0),
        (Decision::Ask, true, 1, 1),
    ] {
        let service = ArtifactService::start().await;
        slides_published_to(&service, "private");
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("chart.png");
        let mut image = png(b"chart");
        if decision == Decision::Deny {
            image.resize(tickloop_agent_artifacts::MAX_IMAGE_BYTES + 1, 0);
        }
        std::fs::write(&path, image).unwrap();
        let rule = read_rule(decision, if is_exact { exact_read(&path) } else { None });
        let model = Arc::new(ScriptedModel::new(vec![
            upload_call("upload", &path),
            text("Finished."),
        ]));
        let agent = signed_in_agent(&service, directory.path(), model.clone()).await;
        let chat = agent.create_chat(directory.path().into()).await.unwrap();
        agent
            .set_execution_mode(chat, ExecutionMode::AcceptEdits)
            .await
            .unwrap();
        agent.set_permission_rules(chat, vec![rule]).await.unwrap();
        let mut events = agent.subscribe();
        agent
            .submit(chat, "Put the chart on the Slides".into())
            .await
            .unwrap();
        let mut prompted = 0;
        loop {
            match tickloop_agent_test_support::next_event(&mut events)
                .await
                .kind
            {
                EventKind::PermissionRequested { request } => {
                    prompted += 1;
                    assert_eq!(request.name, "ArtifactUpload");
                    agent
                        .answer_permission(chat, request.id, true)
                        .await
                        .unwrap();
                }
                EventKind::Completed { .. } => break,
                EventKind::Failed(error) => panic!("{error}"),
                _ => {}
            }
        }
        assert_eq!((prompted, service.uploads().len()), (prompts, sent));
        if sent == 0 {
            let refused = tool_result_text(&model.requests.lock().unwrap()[1], "upload");
            assert!(
                refused.contains("is denied, so the file is not sent")
                    && !refused.contains("exceeds"),
                "the denial comes before the file is measured: {refused}"
            );
        }
        agent.shutdown().await;
    }
}

#[tokio::test]
async fn a_parent_read_denial_frozen_into_the_delegation_refuses_a_subagent_upload() {
    use tickloop_agent_protocol::Decision;
    let service = ArtifactService::start().await;
    slides_published_to(&service, "private");
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("chart.png");
    std::fs::write(&path, png(b"chart")).unwrap();
    let model = Arc::new(ScriptedModel::new(vec![
        call(
            "delegate",
            "Subagent",
            json!({"task":"Put the chart on the Slides"}),
        ),
        call(
            "ready",
            tickloop_agent_tools::ASK_USER_QUESTION_TOOL_NAME,
            json!({"questions":[{"question":"Upload the chart now?","header":"Chart","options":[
                {"label":"Yes","description":"Upload it"},
                {"label":"No","description":"Wait"}
            ]}]}),
        ),
        upload_call("child-upload", &path),
        text("The child could not send it."),
        text("Done."),
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
            vec![read_rule(
                Decision::Deny,
                Some(json!({"file_path": path.canonicalize().unwrap()})),
            )],
        )
        .await
        .unwrap();
    let mut events = agent.subscribe();
    agent
        .submit(parent, "Delegate the chart".into())
        .await
        .unwrap();
    let (child, question) = loop {
        let event = tickloop_agent_test_support::next_event(&mut events).await;
        if let EventKind::QuestionAsked { request } = event.kind {
            break (event.chat_id, request);
        }
    };
    agent
        .set_permission_rules(parent, Vec::new())
        .await
        .unwrap();
    agent
        .answer_question(
            child,
            question.id,
            vec![tickloop_agent_protocol::QuestionAnswer::option("Yes")],
        )
        .await
        .unwrap();
    wait_for_completion(&agent, parent).await;
    assert!(service.uploads().is_empty());
    let refused = tool_result_text(&model.requests.lock().unwrap()[3], "child-upload");
    assert!(
        refused.contains("delegated authorization ceiling denies this operation"),
        "{refused}"
    );
    agent.shutdown().await;
}

#[tokio::test]
async fn a_frozen_parent_session_denial_of_reading_the_file_refuses_a_subagent_upload() {
    use tickloop_agent_protocol::PermissionScope;
    let service = ArtifactService::start().await;
    slides_published_to(&service, "private");
    let directory = tempfile::tempdir().unwrap();
    let elsewhere = tempfile::tempdir().unwrap();
    let path = elsewhere.path().join("photo.png");
    std::fs::write(&path, png(b"photo")).unwrap();
    let model = Arc::new(ScriptedModel::new(vec![
        call("parent-read", "Read", json!({"file_path": path})),
        call(
            "delegate",
            "Subagent",
            json!({"task":"Put the photo on the Slides"}),
        ),
        upload_call("child-upload", &path),
        text("The child could not send it."),
        text("Done."),
    ]));
    let agent = signed_in_agent(&service, directory.path(), model.clone()).await;
    let parent = agent.create_chat(directory.path().into()).await.unwrap();
    agent
        .set_execution_mode(parent, ExecutionMode::AcceptEdits)
        .await
        .unwrap();
    let mut events = agent.subscribe();
    agent.submit(parent, "Read the photo".into()).await.unwrap();
    let request = next_permission(&mut events).await;
    assert_eq!(request.name, "Read");
    agent
        .answer_permission_with_scope(parent, request.id, false, PermissionScope::Session)
        .await
        .unwrap();
    wait_for_loop_state(&agent, parent, LoopState::Interrupted).await;
    agent
        .set_execution_mode(parent, ExecutionMode::FullAccess)
        .await
        .unwrap();
    agent
        .submit(parent, "Delegate the photo".into())
        .await
        .unwrap();
    wait_for_completion(&agent, parent).await;
    assert!(service.uploads().is_empty());
    let refused = tool_result_text(&model.requests.lock().unwrap()[3], "child-upload");
    assert!(
        refused.contains("frozen parent Session denial"),
        "{refused}"
    );
    agent.shutdown().await;
}

#[tokio::test]
async fn auto_mode_uploads_a_file_from_outside_the_workspace_to_private_slides_without_review() {
    let service = ArtifactService::start().await;
    slides_published_to(&service, "private");
    let directory = tempfile::tempdir().unwrap();
    let elsewhere = tempfile::tempdir().unwrap();
    let path = elsewhere.path().join("photo.png");
    std::fs::write(&path, png(b"photo")).unwrap();
    let model = Arc::new(ScriptedModel::new(vec![
        upload_call("upload", &path),
        text("Uploaded."),
    ]));
    let agent = signed_in_agent(&service, directory.path(), model).await;
    let reviewer = Arc::new(ScriptedModel::new(vec![text(
        &tickloop_agent_test_support::auto_classification(false).to_string(),
    )]));
    agent.set_auto_reviewer(reviewer.clone()).await;
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    agent
        .set_execution_mode(chat, ExecutionMode::Auto)
        .await
        .unwrap();
    let mut events = agent.subscribe();
    agent
        .submit(chat, "Add my photo to the Slides".into())
        .await
        .unwrap();
    loop {
        match tickloop_agent_test_support::next_event(&mut events)
            .await
            .kind
        {
            EventKind::ApprovalAssessed { .. } => panic!("the upload needs no review"),
            EventKind::PermissionRequested { .. } => panic!("the upload needs no approval"),
            EventKind::Completed { .. } => break,
            EventKind::Failed(error) => panic!("{error}"),
            _ => {}
        }
    }
    assert!(reviewer.requests.lock().unwrap().is_empty());
    assert_eq!(service.uploads().len(), 1);
    agent.shutdown().await;
}

#[tokio::test]
async fn shared_slides_uploads_are_reviewable_and_plan_mode_refuses_them() {
    for (mode, prompts, sent) in [
        (ExecutionMode::AcceptEdits, 1, 1),
        (ExecutionMode::Plan, 0, 0),
    ] {
        let service = ArtifactService::start().await;
        slides_published_to(&service, "shared");
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("chart.png");
        std::fs::write(&path, png(b"chart")).unwrap();
        let model = Arc::new(ScriptedModel::new(vec![
            upload_call("upload", &path),
            text("Finished."),
        ]));
        let agent = signed_in_agent(&service, directory.path(), model).await;
        let chat = agent.create_chat(directory.path().into()).await.unwrap();
        agent.set_execution_mode(chat, mode).await.unwrap();
        let mut events = agent.subscribe();
        agent
            .submit(chat, "Add the chart to the team's Slides".into())
            .await
            .unwrap();
        let mut prompted = 0;
        loop {
            match tickloop_agent_test_support::next_event(&mut events)
                .await
                .kind
            {
                EventKind::PermissionRequested { request } => {
                    prompted += 1;
                    agent
                        .answer_permission(chat, request.id, true)
                        .await
                        .unwrap();
                }
                EventKind::Completed { .. } => break,
                EventKind::Failed(error) => panic!("{error}"),
                _ => {}
            }
        }
        assert_eq!(
            (prompted, service.uploads().len()),
            (prompts, sent),
            "{mode:?}"
        );
        agent.shutdown().await;
    }
}

#[tokio::test]
async fn public_slides_ask_before_the_first_upload_and_continue_without_asking() {
    let service = ArtifactService::start().await;
    slides_published_to(&service, "public");
    let directory = tempfile::tempdir().unwrap();
    let first = directory.path().join("chart.png");
    std::fs::write(&first, png(b"chart")).unwrap();
    let second = directory.path().join("logo.png");
    std::fs::write(&second, png(b"logo")).unwrap();
    let model = Arc::new(ScriptedModel::new(vec![
        upload_call("first", &first),
        upload_call("second", &second),
        text("Both uploaded."),
    ]));
    let agent = signed_in_agent(&service, directory.path(), model).await;
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    agent
        .set_execution_mode(chat, ExecutionMode::AcceptEdits)
        .await
        .unwrap();
    let mut events = agent.subscribe();
    agent
        .submit(chat, "Add both images to the public Slides".into())
        .await
        .unwrap();
    let permission = next_permission(&mut events).await;
    assert_eq!(
        permission.input["file_path"],
        json!(first.canonicalize().unwrap())
    );
    agent
        .answer_permission(chat, permission.id, true)
        .await
        .unwrap();
    loop {
        match tickloop_agent_test_support::next_event(&mut events)
            .await
            .kind
        {
            EventKind::PermissionRequested { .. } => {
                panic!("the admitted public audience continues without asking")
            }
            EventKind::Completed { .. } => break,
            EventKind::Failed(error) => panic!("{error}"),
            _ => {}
        }
    }
    assert_eq!(service.uploads().len(), 2);
    agent.shutdown().await;
}

#[tokio::test]
async fn an_audience_change_after_approval_stops_the_upload_before_it_is_sent() {
    let service = ArtifactService::start().await;
    slides_published_to(&service, "private");
    let directory = tempfile::tempdir().unwrap();
    let elsewhere = tempfile::tempdir().unwrap();
    let path = elsewhere.path().join("photo.png");
    std::fs::write(&path, png(b"photo")).unwrap();
    let model = Arc::new(ScriptedModel::new(vec![
        upload_call("upload", &path),
        text("Not uploaded."),
    ]));
    let agent = signed_in_agent(&service, directory.path(), model.clone()).await;
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    agent
        .set_execution_mode(chat, ExecutionMode::AcceptEdits)
        .await
        .unwrap();
    let mut events = agent.subscribe();
    agent
        .submit(chat, "Add my photo to the Slides".into())
        .await
        .unwrap();
    let permission = next_permission(&mut events).await;
    slides_published_to(&service, "public");
    agent
        .answer_permission(chat, permission.id, true)
        .await
        .unwrap();
    wait_for_completion(&agent, chat).await;
    assert!(service.uploads().is_empty());
    let refused = tool_result_text(&model.requests.lock().unwrap()[1], "upload");
    assert!(refused.contains("Artifact audience changed"), "{refused}");
    agent.shutdown().await;
}

#[tokio::test]
async fn a_read_ask_added_after_approval_stops_the_upload_before_it_is_sent() {
    use tickloop_agent_protocol::Decision;
    let service = ArtifactService::start().await;
    slides_published_to(&service, "private");
    let directory = tempfile::tempdir().unwrap();
    let elsewhere = tempfile::tempdir().unwrap();
    let path = elsewhere.path().join("photo.png");
    std::fs::write(&path, png(b"photo")).unwrap();
    let model = Arc::new(ScriptedModel::new(vec![
        upload_call("upload", &path),
        text("Not uploaded."),
    ]));
    let agent = signed_in_agent(&service, directory.path(), model.clone()).await;
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    agent
        .set_execution_mode(chat, ExecutionMode::AcceptEdits)
        .await
        .unwrap();
    let mut events = agent.subscribe();
    agent
        .submit(chat, "Add my photo to the Slides".into())
        .await
        .unwrap();
    let permission = next_permission(&mut events).await;
    // The upload itself stays allowed outright; only its Read now asks.
    let mut allow_upload = read_rule(Decision::Allow, None);
    allow_upload.tool = "ArtifactUpload".into();
    agent
        .set_permission_rules(
            chat,
            vec![
                allow_upload,
                read_rule(
                    Decision::Ask,
                    Some(json!({"file_path": path.canonicalize().unwrap()})),
                ),
            ],
        )
        .await
        .unwrap();
    agent
        .answer_permission(chat, permission.id, true)
        .await
        .unwrap();
    wait_for_completion(&agent, chat).await;
    assert!(service.uploads().is_empty());
    let refused = tool_result_text(&model.requests.lock().unwrap()[1], "upload");
    assert!(
        refused.contains("now needs approval, so the file was not sent"),
        "{refused}"
    );
    agent.shutdown().await;
}

#[tokio::test]
async fn an_upload_cancelled_after_it_was_sent_is_unknown_and_never_sent_again() {
    let service = ArtifactService::start().await;
    slides_published_to(&service, "private");
    service
        .control
        .hold_upload_response
        .store(true, Ordering::SeqCst);
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("chart.png");
    std::fs::write(&path, png(b"chart")).unwrap();
    let model = Arc::new(ScriptedModel::new(vec![
        upload_call("upload", &path),
        text("Stopped."),
    ]));
    let agent = signed_in_agent(&service, directory.path(), model).await;
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    agent
        .set_execution_mode(chat, ExecutionMode::FullAccess)
        .await
        .unwrap();
    agent.submit(chat, "Add the chart".into()).await.unwrap();
    tokio::time::timeout(WAIT, service.control.upload_held.notified())
        .await
        .unwrap();
    agent.interrupt(chat).await.unwrap();
    wait_for_loop_state(&agent, chat, LoopState::Interrupted).await;
    let operation = agent
        .snapshot(chat)
        .await
        .unwrap()
        .operations
        .into_iter()
        .find(|operation| operation.name == "ArtifactUpload")
        .unwrap();
    assert_eq!(operation.state, OperationState::Unknown);
    let content = operation.result.unwrap().content;
    assert!(
        content.contains("cancelled after it was sent")
            && content.contains("read the Slides with ArtifactRead"),
        "{content}"
    );
    assert_eq!(service.uploads().len(), 1);
    assert_eq!(
        service
            .lifecycle_requests("POST /artifact/api/v1/artifacts/deck/assets?operation_id=")
            .len(),
        1
    );
    assert!(
        service
            .lifecycle_requests("GET /artifact/api/v1/artifacts/deck/operations/")
            .is_empty()
    );
    agent.shutdown().await;
}

#[tokio::test]
async fn a_busy_service_refusal_tells_the_model_it_may_upload_again_after_a_moment() {
    let service = ArtifactService::start().await;
    slides_published_to(&service, "private");
    *service.control.refuse_uploads.lock().unwrap() = Some((
        429,
        "asset_busy".into(),
        "These Slides already have 4 uploads in progress".into(),
    ));
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("chart.png");
    std::fs::write(&path, png(b"chart")).unwrap();
    let model = Arc::new(ScriptedModel::new(vec![
        upload_call("upload", &path),
        text("Will retry."),
    ]));
    let agent = signed_in_agent(&service, directory.path(), model.clone()).await;
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    agent
        .set_execution_mode(chat, ExecutionMode::FullAccess)
        .await
        .unwrap();
    submit_and_wait(&agent, chat, "Add the chart").await;
    let refused = tool_result_text(&model.requests.lock().unwrap()[1], "upload");
    assert_eq!(
        refused,
        "Tickloop Artifacts refused the upload (HTTP 429 asset_busy): These Slides already have 4 uploads in progress. Nothing was stored; wait a moment, then upload the same file again."
    );
    agent.shutdown().await;
}

#[tokio::test]
async fn slides_guidance_continues_into_the_next_run_after_sign_in_and_no_further() {
    const SLIDES_GUIDANCE: &str = "The user ran /slides to make a new Slides artifact.";
    let service = ArtifactService::start().await;
    service
        .control
        .hold_device_approval
        .store(true, Ordering::SeqCst);
    let directory = tempfile::tempdir().unwrap();
    let model = Arc::new(ScriptedModel::new(vec![
        call("login", "ArtifactLogin", json!({})),
        text("Open the link and confirm the code, then tell me."),
        call(
            "create",
            "ArtifactEdit",
            json!({"action":"create","title":"Q3 review","format":"slides","content":"<section id=\"cover\" style=\"background:#ffffff\"><h1>Q3 review</h1></section>"}),
        ),
        text("Created."),
        text("You're welcome."),
    ]));
    let agent = Agent::new_embedded(config(), model.clone());
    agent
        .install_host_tools(Arc::new(McpHostToolProvider::new(
            service.client(directory.path()),
            None,
        )))
        .unwrap();
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    let invocation = agent
        .invoke_skill(chat, "slides", "the Q3 review")
        .await
        .unwrap();
    wait_for_completion(&agent, chat).await;
    service
        .control
        .hold_device_approval
        .store(false, Ordering::SeqCst);
    let account = service.client(directory.path());
    eventually(async || matches!(account.credential_snapshot(), Ok(Some(_)))).await;
    for follow_up in ["Signed in", "Thanks"] {
        agent.submit(chat, follow_up.into()).await.unwrap();
        // The created Slides keep the Loop receiving their @tickloop requests.
        eventually(async || {
            let chat_loop = agent.snapshot(chat).await.unwrap().chat_loop;
            chat_loop.id == invocation.loop_id
                && chat_loop.state == LoopState::Idle
                && !chat_loop.event_sources.is_empty()
        })
        .await;
    }
    agent.shutdown().await;
    let requests = model.requests.lock().unwrap();
    assert_eq!(requests.len(), 5);
    let guided = requests
        .iter()
        .map(|request| {
            let guided = guidance_in_effect(request, "slides");
            assert!(!guided || mentions(request, SLIDES_GUIDANCE) == 1);
            guided
        })
        .collect::<Vec<_>>();
    assert_eq!(
        guided,
        [true, true, true, true, false],
        "the run after sign-in carries /slides; the run after it does not"
    );
    assert!(tool_names(&requests[2]).contains(&"ArtifactEdit"));
    let calls = service.tool_calls();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0]["arguments"]["format"], "slides");
}

/// Refuses every local file read; the host calls these tests prepare send no file.
struct NoLocalReads;

#[async_trait]
impl tickloop_agent_tools::LocalReadAdmission for NoLocalReads {
    async fn admit(&self, _: &tickloop_agent_tools::PreparedTool) -> Result<bool, String> {
        Err("no local file is read here".into())
    }
}

fn expire_artifacts_tokens(root: &std::path::Path) {
    let credentials = std::fs::read_dir(root.join("artifacts/auth"))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .find(|path| {
            path.extension()
                .is_some_and(|extension| extension == "json")
        })
        .unwrap();
    let mut saved: Value = serde_json::from_slice(&std::fs::read(&credentials).unwrap()).unwrap();
    for token in saved["resources"].as_object_mut().unwrap().values_mut() {
        token["expires_at"] = json!(0);
    }
    std::fs::write(&credentials, serde_json::to_vec(&saved).unwrap()).unwrap();
}

#[tokio::test]
async fn refreshing_an_expiring_accounts_resource_keeps_tools_available_in_the_same_refresh() {
    let service = ArtifactService::start().await;
    let directory = tempfile::tempdir().unwrap();
    let account = service.login(directory.path()).await;
    let provider = McpHostToolProvider::new(account, None);
    provider.refresh(CancellationToken::new()).await.unwrap();
    expire_artifacts_tokens(directory.path());
    service
        .control
        .credential_revision
        .store(1, Ordering::SeqCst);
    provider.refresh(CancellationToken::new()).await.unwrap();
    assert_eq!(
        provider.service_connections()["artifacts"]["authentication"],
        "signed_in"
    );
    assert!(
        provider
            .definitions()
            .iter()
            .any(|tool| tool.name == "ArtifactRead")
    );
}

struct HostTokens {
    token: Mutex<String>,
    invalidations: AtomicUsize,
}

#[async_trait]
impl tickloop_agent_artifacts::ArtifactCredentials for HostTokens {
    async fn bearer(&self) -> Result<Option<String>, tickloop_agent_artifacts::ArtifactError> {
        Ok(Some(self.token.lock().unwrap().clone()))
    }
    async fn invalidate(
        &self,
        rejected: &str,
    ) -> Result<(), tickloop_agent_artifacts::ArtifactError> {
        self.invalidations.fetch_add(1, Ordering::SeqCst);
        let mut token = self.token.lock().unwrap();
        if *token == rejected {
            *token = "renewed-chat-token".into();
        }
        Ok(())
    }
}

/// A host source that scopes tokens by Chat: every Chat of the Agent acts as itself.
struct ChatScopedTokens {
    chat: Option<uuid::Uuid>,
}

#[async_trait]
impl tickloop_agent_artifacts::ArtifactCredentials for ChatScopedTokens {
    async fn bearer(&self) -> Result<Option<String>, tickloop_agent_artifacts::ArtifactError> {
        Ok(Some(match self.chat {
            Some(chat) => format!("chat-{chat}"),
            None => "family-token".into(),
        }))
    }

    fn for_chat(
        &self,
        chat_id: uuid::Uuid,
    ) -> Option<Arc<dyn tickloop_agent_artifacts::ArtifactCredentials>> {
        Some(Arc::new(ChatScopedTokens {
            chat: Some(chat_id),
        }))
    }
}

#[tokio::test]
async fn each_chat_links_an_artifact_with_its_own_host_credentials() {
    let service = ArtifactService::start().await;
    let directory = tempfile::tempdir().unwrap();
    let provider = Arc::new(
        McpHostToolProvider::with_credentials(
            &service.origin,
            "user-one",
            Arc::new(ChatScopedTokens { chat: None }),
            None,
        )
        .unwrap(),
    );
    provider.refresh(CancellationToken::new()).await.unwrap();
    let agent = Agent::new_embedded(config(), Arc::new(ScriptedModel::new(vec![])));
    agent.install_host_tools(provider).unwrap();
    let mut chats = Vec::new();
    for _ in 0..2 {
        chats.push(agent.create_chat(directory.path().into()).await.unwrap());
    }
    for chat in &chats {
        agent
            .link_artifact(
                *chat,
                &artifact_tick(&service, "doc"),
                tickloop_agent_protocol::ArtifactFeedbackHistory::NewOnly,
            )
            .await
            .unwrap();
    }
    let links: Vec<String> = service
        .requests()
        .into_iter()
        .filter(|request| {
            request["path"]
                .as_str()
                .unwrap()
                .contains("/api/v1/agent/links/")
        })
        .map(|request| request["headers"].as_str().unwrap().to_ascii_lowercase())
        .collect();
    assert_eq!(links.len(), 2, "one link per Chat");
    for (headers, chat) in links.iter().zip(&chats) {
        assert!(
            headers.contains(&format!("authorization: bearer chat-{chat}")),
            "{headers}"
        );
    }
    agent.shutdown().await;
}

#[tokio::test]
async fn separate_chat_harnesses_use_their_injected_credentials_for_read_edit_and_comment() {
    for token in ["first-chat-token", "second-chat-token"] {
        let service = ArtifactService::start().await;
        let directory = tempfile::tempdir().unwrap();
        let source = Arc::new(HostTokens {
            token: Mutex::new(token.into()),
            invalidations: AtomicUsize::new(0),
        });
        let provider = Arc::new(
            McpHostToolProvider::with_credentials(
                &service.origin,
                "user-one",
                source.clone(),
                None,
            )
            .unwrap(),
        );
        let model = Arc::new(ScriptedModel::new(vec![
            call(
                "read",
                "ArtifactRead",
                json!({"action":"read","artifact_id":"doc"}),
            ),
            call(
                "edit",
                "ArtifactEdit",
                json!({"action":"patch","artifact_id":"doc","patches":[{"type":"replace","target":"b1","expected_hash":"h1","text":"# Edited"}]}),
            ),
            call(
                "comment",
                "ArtifactComment",
                json!({"action":"reply","artifact_id":"doc","thread_id":"thread-1","text":"Updated."}),
            ),
            text("Done."),
        ]));
        let agent = Agent::new_embedded(config(), model);
        agent.install_host_tools(provider).unwrap();
        let chat = agent.create_chat(directory.path().into()).await.unwrap();
        agent
            .set_execution_mode(chat, ExecutionMode::FullAccess)
            .await
            .unwrap();
        submit_and_wait(&agent, chat, "Read, edit and comment on this artifact").await;
        let calls = service
            .requests()
            .into_iter()
            .filter(|request| request["body"]["method"] == "tools/call")
            .collect::<Vec<_>>();
        assert_eq!(
            calls
                .iter()
                .map(|request| request["body"]["params"]["name"].as_str().unwrap())
                .collect::<Vec<_>>(),
            ["ArtifactRead", "ArtifactEdit", "ArtifactComment"]
        );
        assert!(calls.iter().all(|request| {
            request["headers"]
                .as_str()
                .unwrap()
                .to_ascii_lowercase()
                .contains(&format!("authorization: bearer {token}"))
        }));
        assert!(
            !service
                .requests()
                .iter()
                .any(|request| request["path"].as_str().unwrap().contains("/account/"))
        );
        assert!(!directory.path().join("artifacts/auth").exists());
        assert_eq!(source.invalidations.load(Ordering::SeqCst), 0);
        agent.shutdown().await;
    }
}

#[tokio::test]
async fn delegated_mcp_401_invalidates_and_retries_the_same_read_edit_and_comment_once() {
    for (name, arguments) in [
        ("ArtifactRead", json!({"action":"read","artifact_id":"doc"})),
        (
            "ArtifactEdit",
            json!({"action":"patch","artifact_id":"doc","patches":[{"type":"replace","target":"b1","expected_hash":"h1","text":"# Edited"}]}),
        ),
        (
            "ArtifactComment",
            json!({"action":"reply","artifact_id":"doc","thread_id":"thread-1","text":"Updated."}),
        ),
    ] {
        let service = ArtifactService::start().await;
        *service.control.refuse_tool_bearer.lock().unwrap() = Some("expired-chat-token".into());
        let source = Arc::new(HostTokens {
            token: Mutex::new("expired-chat-token".into()),
            invalidations: AtomicUsize::new(0),
        });
        let provider = McpHostToolProvider::with_credentials(
            &service.origin,
            "user-one",
            source.clone(),
            None,
        )
        .unwrap();
        provider.refresh(CancellationToken::new()).await.unwrap();
        let chat = uuid::Uuid::new_v4();
        let prepared = provider
            .prepare_call(chat, name, &arguments, CancellationToken::new())
            .await
            .unwrap();
        let outcome = provider
            .execute_prepared(chat, prepared, CancellationToken::new())
            .await;
        let result = match outcome {
            HostCallResult::Completed(result) | HostCallResult::CompletedWithEffect(result, _) => {
                result
            }
            _ => panic!("a rejected bearer is renewed and retried once"),
        };
        assert!(!result.is_error, "{}", result.content);
        let calls = service
            .requests()
            .into_iter()
            .filter(|request| request["body"]["method"] == "tools/call")
            .collect::<Vec<_>>();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0]["body"], calls[1]["body"]);
        assert!(
            calls[1]["headers"]
                .as_str()
                .unwrap()
                .to_ascii_lowercase()
                .contains("authorization: bearer renewed-chat-token")
        );
        assert_eq!(source.invalidations.load(Ordering::SeqCst), 1);
    }
}

#[tokio::test]
async fn a_folderless_durable_chat_excludes_local_skills_and_loads_remote_skills() {
    use tickloop_agent_protocol::{AgentHostOptions, PromptPolicy};
    use tickloop_agent_runtime::{HarnessOptions, HarnessSeams, ToolPolicy, WaitHosting};
    use tickloop_agent_test_support::harness::ChosenModels;
    let service = ArtifactService::start().await;
    service.control.skills_enabled.store(true, Ordering::SeqCst);
    let directory = tempfile::tempdir().unwrap();
    let account = service.login(directory.path()).await;
    let model = Arc::new(ScriptedModel::new(vec![
        call("builtin", "Skill", json!({"skill": "tickloop"})),
        call(
            "remote",
            "Skill",
            json!({"skill": "tickloop-artifacts:artifact-slides"}),
        ),
        text("Ready"),
    ]));
    let agent = Agent::with_seams(
        HarnessSeams {
            transcripts: None,
            environment: None,
            models: ChosenModels::new(model.clone()),
        },
        HarnessOptions {
            search: tickloop_agent_runtime::SearchDefaults::Fixed(None),
            host: AgentHostOptions {
                prompt_policy: PromptPolicy::DenyOnPrompt,
                ..AgentHostOptions::embedded_defaults()
            },
            tools: ToolPolicy {
                web_search: true,
                ..ToolPolicy::durable_chat()
            },
            host_rules: tickloop_agent_runtime::durable_chat_host_rules(),
            waits: WaitHosting::Durable,
            ..HarnessOptions::local(config())
        },
    )
    .unwrap();
    agent
        .install_host_tools(Arc::new(McpHostToolProvider::new(account, None)))
        .unwrap();
    let chat = agent.create_chat_without_folder().await.unwrap();
    assert!(agent.invoke_skill(chat, "slides", "").await.is_err());
    assert!(agent.snapshot(chat).await.unwrap().messages.is_empty());
    submit_and_wait(&agent, chat, "Prepare slides").await;
    let snapshot = agent.snapshot(chat).await.unwrap();
    assert!(
        !snapshot
            .invoked_skills
            .iter()
            .any(|skill| skill.name == "tickloop")
    );
    assert!(snapshot.messages.iter().flat_map(|message| &message.content).any(|content| matches!(content, Content::ToolResult {tool_use_id, is_error: true, ..} if tool_use_id == "builtin")));
    assert!(
        snapshot
            .invoked_skills
            .iter()
            .any(|skill| skill.name == "tickloop-artifacts:artifact-slides")
    );
    let requests = model.requests.lock().unwrap().clone();
    let prompt = requests[0].messages[0]
        .content
        .iter()
        .find_map(|c| {
            if let Content::Text { text } = c {
                Some(text)
            } else {
                None
            }
        })
        .unwrap();
    let state: Value =
        serde_json::from_str(prompt.split_once("Current host state: ").unwrap().1).unwrap();
    for field in [
        "loop",
        "plan",
        "execution_mode",
        "service_connections",
        "artifact_ticks",
        "web_search",
        "delegation",
        "family_members",
    ] {
        assert!(state.get(field).is_some(), "{field}");
    }
    assert_eq!(
        state["service_connections"]["artifacts"]["authentication"],
        "signed_in"
    );
    assert!(prompt.contains("# Web search is disabled"));
    assert!(prompt.contains("# Artifacts"));
    for absent in [
        "workdir",
        "family_temp_directory",
        "scratchpad",
        "workspace_host",
    ] {
        assert!(state.get(absent).is_none(), "{absent}");
    }
    for absent in ["Tickhost", "Herdr", "Pane"] {
        assert!(!prompt.contains(absent), "{absent}");
    }
    agent.shutdown().await;
}
