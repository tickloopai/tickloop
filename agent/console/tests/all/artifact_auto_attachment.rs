use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{Value, json};
use tickloop_agent_protocol::{ArtifactTick, ChatId, EventKind, ToolDefinition, ToolResult};
use tickloop_agent_runtime::{
    Agent,
    host_tools::{
        HostCallResult, HostEffect, HostToolPermission, HostToolProvider, PreparedHostCall,
    },
};
use tokio_util::sync::CancellationToken;

struct CreatingArtifact;

#[async_trait]
impl HostToolProvider for CreatingArtifact {
    fn definitions(&self) -> Vec<ToolDefinition> {
        vec![ToolDefinition {
            name: "create_custom_document".into(),
            description: "Create a document".into(),
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
                content: "Created".into(),
                is_error: false,
                output_ref: None,
            },
            Box::new(HostEffect::CreatedArtifact(artifact("Created title"))),
        )
    }
}

fn artifact(title: &str) -> ArtifactTick {
    ArtifactTick {
        artifact_id: "document-one".into(),
        origin: "https://docs.example".into(),
        title: title.into(),
        format: "markdown".into(),
        url: "https://docs.example/a/document-one".into(),
        access_role: "owner".into(),
        added_at_ms: 1,
    }
}

async fn create(agent: &Agent, chat: ChatId) {
    let mut events = agent.subscribe();
    agent
        .submit(chat, None, "Create the document".into())
        .await
        .unwrap();
    loop {
        match tickloop_agent_test_support::next_event(&mut events)
            .await
            .kind
        {
            EventKind::Completed { .. } => break,
            EventKind::Failed(error) => panic!("{error}"),
            _ => {}
        }
    }
}

#[tokio::test]
async fn a_created_artifact_attaches_to_its_chat_without_creating_a_subscription() {
    let directory = tempfile::tempdir().unwrap();
    let model = Arc::new(tickloop_agent_test_support::ScriptedModel::new(vec![
        tickloop_agent_test_support::call("create", "create_custom_document", json!({})),
        tickloop_agent_test_support::text("done"),
    ]));
    let agent = Agent::new_embedded(tickloop_agent_test_support::config(), model);
    agent
        .install_host_tools(Arc::new(CreatingArtifact))
        .unwrap();
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    let other = agent.create_chat(directory.path().into()).await.unwrap();
    create(&agent, chat).await;
    assert_eq!(
        agent.artifact_ticks(chat).await.unwrap(),
        vec![artifact("Created title")]
    );
    let monitor = tickloop_agent_console::monitor_rows::MonitorRowsRegistry::builtin();
    let projected = monitor.list(&tickloop_agent_console::monitor_rows::MonitorContext {
        chat: Some(agent.snapshot(chat).await.unwrap()),
        ..Default::default()
    });
    assert_eq!(projected.len(), 1);
    assert_eq!(projected[0].title, "Created title");
    assert_eq!(
        projected[0].instance.as_str(),
        artifact("title").instance_id()
    );
    assert!(
        monitor
            .list(&tickloop_agent_console::monitor_rows::MonitorContext {
                chat: Some(agent.snapshot(other).await.unwrap()),
                ..Default::default()
            })
            .is_empty()
    );
    assert!(agent.artifact_ticks(other).await.unwrap().is_empty());
    assert!(agent.tick_subscriptions(chat).await.unwrap().is_empty());
    agent.shutdown().await;
}

#[tokio::test]
async fn manual_removal_prevents_a_later_creation_effect_after_restart_and_branch() {
    let directory = tempfile::tempdir().unwrap();
    let store = directory.path().join("archive");
    let response = || {
        vec![
            tickloop_agent_test_support::call("create", "create_custom_document", json!({})),
            tickloop_agent_test_support::text("done"),
        ]
    };
    let agent = Agent::open_embedded(
        store.clone(),
        tickloop_agent_test_support::config(),
        Arc::new(tickloop_agent_test_support::ScriptedModel::new(response())),
    )
    .unwrap();
    agent
        .install_host_tools(Arc::new(CreatingArtifact))
        .unwrap();
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    create(&agent, chat).await;
    agent
        .remove_artifact_tick(chat, &artifact("title").instance_id())
        .await
        .unwrap();
    agent.shutdown().await;
    drop(agent);
    let restored = Agent::open_embedded(
        store,
        tickloop_agent_test_support::config(),
        Arc::new(tickloop_agent_test_support::ScriptedModel::new(
            [response(), response(), response()].concat(),
        )),
    )
    .unwrap();
    restored
        .install_host_tools(Arc::new(CreatingArtifact))
        .unwrap();
    restored.resume_chat(chat).await.unwrap();
    create(&restored, chat).await;
    assert!(restored.artifact_ticks(chat).await.unwrap().is_empty());
    let branch = restored
        .branch_chat(tickloop_agent_protocol::BranchRequest {
            request_id: uuid::Uuid::new_v4(),
            source_chat_id: chat,
            source_loop_id: None,
            name: Some("Removed artifact branch".into()),
        })
        .await
        .unwrap();
    create(&restored, branch.child_chat_id).await;
    assert!(
        restored
            .artifact_ticks(branch.child_chat_id)
            .await
            .unwrap()
            .is_empty()
    );
    restored
        .add_artifact_tick(chat, artifact("Explicit re-add"))
        .await
        .unwrap();
    create(&restored, chat).await;
    assert_eq!(restored.artifact_ticks(chat).await.unwrap().len(), 1);
    restored.shutdown().await;
}

struct ArtifactEffects(std::sync::Mutex<std::collections::VecDeque<HostCallResult>>);
#[async_trait]
impl HostToolProvider for ArtifactEffects {
    fn definitions(&self) -> Vec<ToolDefinition> {
        CreatingArtifact.definitions()
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
        self.0.lock().unwrap().pop_front().unwrap()
    }
}

#[tokio::test]
async fn repeated_confirmed_effects_refresh_metadata_but_failure_and_unknown_do_not_attach() {
    let directory = tempfile::tempdir().unwrap();
    let model = Arc::new(tickloop_agent_test_support::ScriptedModel::new(
        (0..4)
            .flat_map(|index| {
                vec![
                    tickloop_agent_test_support::call(
                        &format!("effect-{index}"),
                        "create_custom_document",
                        json!({}),
                    ),
                    tickloop_agent_test_support::text("done"),
                ]
            })
            .collect(),
    ));
    let agent = Agent::new_embedded(tickloop_agent_test_support::config(), model);
    let mut created = artifact("Created title");
    created.added_at_ms = 17;
    let mut refreshed = artifact("Updated title");
    refreshed.format = "html".into();
    refreshed.url = "https://docs.example/a/document-one?published=1".into();
    let completed = |tick, error| {
        HostCallResult::CompletedWithEffect(
            ToolResult {
                content: "result".into(),
                is_error: error,
                output_ref: None,
            },
            Box::new(HostEffect::CreatedArtifact(tick)),
        )
    };
    agent
        .install_host_tools(Arc::new(ArtifactEffects(std::sync::Mutex::new(
            [
                completed(created.clone(), false),
                completed(refreshed.clone(), false),
                completed(artifact("Rejected replacement"), true),
                HostCallResult::OutcomeUnknown("No confirmed reply".into()),
            ]
            .into(),
        ))))
        .unwrap();
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    create(&agent, chat).await;
    assert_eq!(agent.artifact_ticks(chat).await.unwrap(), vec![created]);
    create(&agent, chat).await;
    refreshed.added_at_ms = 17;
    assert_eq!(
        agent.artifact_ticks(chat).await.unwrap(),
        vec![refreshed.clone()]
    );
    create(&agent, chat).await;
    create(&agent, chat).await;
    assert_eq!(agent.artifact_ticks(chat).await.unwrap(), vec![refreshed]);
    assert!(
        agent
            .snapshot(chat)
            .await
            .unwrap()
            .loops
            .iter()
            .all(|goal| goal.event_sources.is_empty())
    );
    agent.shutdown().await;
}
