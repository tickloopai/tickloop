mod support;

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{Value, json};
use tickloop_agent::{
    artifacts::ArtifactTick,
    host_tools::{
        HostCallResult, HostEffect, HostToolPermission, HostToolProvider, PreparedHostCall,
        PublishedSourceEffect,
    },
    protocol::{ChatId, ToolDefinition},
    runtime::{Agent, EventKind},
    tools::ToolResult,
};
use tokio_util::sync::CancellationToken;

struct PublishingArtifact;

#[async_trait]
impl HostToolProvider for PublishingArtifact {
    fn definitions(&self) -> Vec<ToolDefinition> {
        vec![ToolDefinition {
            name: "release_custom_document".into(),
            description: "Publish a document".into(),
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
        let tick = artifact("Published title");
        let effect = PublishedSourceEffect {
            server_id: tick.origin.clone(),
            owner_account_id: "owner".into(),
            source_key: tick.artifact_id.clone(),
            operation_id: "publish-op".into(),
            version_id: "version-one".into(),
            credential_generation: [0; 32],
            directory_fingerprint: [0; 32],
            bridge_fingerprint: [0; 32],
            artifact_tick: Some(tick),
        };
        HostCallResult::CompletedWithEffect(
            ToolResult {
                content: "Published".into(),
                is_error: false,
                output_ref: None,
            },
            Box::new(HostEffect::PublishedSource(effect)),
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

async fn publish(agent: &Agent, chat: ChatId) {
    let mut events = agent.subscribe();
    agent
        .submit(chat, None, "Publish the document".into())
        .await
        .unwrap();
    loop {
        match support::next_event(&mut events).await.kind {
            EventKind::Completed { .. } => break,
            EventKind::Failed(error) => panic!("{error}"),
            _ => {}
        }
    }
}

#[tokio::test]
async fn confirmed_publication_attaches_to_its_chat_without_creating_a_subscription() {
    let directory = tempfile::tempdir().unwrap();
    let model = Arc::new(support::ScriptedModel::new(vec![
        support::call("publish", "release_custom_document", json!({})),
        support::text("done"),
    ]));
    let agent = Agent::new_embedded(support::config(), model);
    agent
        .install_host_tools(Arc::new(PublishingArtifact))
        .unwrap();
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    let other = agent.create_chat(directory.path().into()).await.unwrap();
    publish(&agent, chat).await;
    assert_eq!(
        agent.artifact_ticks(chat).await.unwrap(),
        vec![artifact("Published title")]
    );
    let adapters = tickloop_agent::tick_adapter::TickAdapters::presentation();
    let projected = adapters.list(&tickloop_agent::tick_adapter::MonitorContext {
        chat: Some(agent.snapshot(chat).await.unwrap()),
        ..Default::default()
    });
    assert_eq!(projected.len(), 1);
    assert_eq!(projected[0].title, "Published title");
    assert_eq!(
        projected[0].instance.as_str(),
        artifact("title").instance_id()
    );
    assert!(
        adapters
            .list(&tickloop_agent::tick_adapter::MonitorContext {
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
async fn manual_removal_prevents_later_publication_after_restart_and_branch() {
    let directory = tempfile::tempdir().unwrap();
    let store = directory.path().join("archive");
    let response = || {
        vec![
            support::call("publish", "release_custom_document", json!({})),
            support::text("done"),
        ]
    };
    let agent = Agent::open_embedded(
        store.clone(),
        support::config(),
        Arc::new(support::ScriptedModel::new(response())),
    )
    .unwrap();
    agent
        .install_host_tools(Arc::new(PublishingArtifact))
        .unwrap();
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    publish(&agent, chat).await;
    agent
        .remove_artifact_tick(chat, &artifact("title").instance_id())
        .await
        .unwrap();
    agent.shutdown().await;
    drop(agent);
    let restored = Agent::open_embedded(
        store,
        support::config(),
        Arc::new(support::ScriptedModel::new(
            [response(), response(), response()].concat(),
        )),
    )
    .unwrap();
    restored
        .install_host_tools(Arc::new(PublishingArtifact))
        .unwrap();
    restored.resume_chat(chat).await.unwrap();
    publish(&restored, chat).await;
    assert!(restored.artifact_ticks(chat).await.unwrap().is_empty());
    let branch = restored
        .branch_chat(tickloop_agent::runtime::BranchRequest {
            request_id: uuid::Uuid::new_v4(),
            source_chat_id: chat,
            source_loop_id: None,
            name: Some("Removed artifact branch".into()),
        })
        .await
        .unwrap();
    publish(&restored, branch.child_chat_id).await;
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
    publish(&restored, chat).await;
    assert_eq!(restored.artifact_ticks(chat).await.unwrap().len(), 1);
    restored.shutdown().await;
}

struct ArtifactEffects(std::sync::Mutex<std::collections::VecDeque<HostCallResult>>);
#[async_trait]
impl HostToolProvider for ArtifactEffects {
    fn definitions(&self) -> Vec<ToolDefinition> {
        PublishingArtifact.definitions()
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
    let model = Arc::new(support::ScriptedModel::new(
        (0..4)
            .flat_map(|index| {
                vec![
                    support::call(
                        &format!("effect-{index}"),
                        "release_custom_document",
                        json!({}),
                    ),
                    support::text("done"),
                ]
            })
            .collect(),
    ));
    let agent = Agent::new_embedded(support::config(), model);
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
    publish(&agent, chat).await;
    assert_eq!(agent.artifact_ticks(chat).await.unwrap(), vec![created]);
    publish(&agent, chat).await;
    refreshed.added_at_ms = 17;
    assert_eq!(
        agent.artifact_ticks(chat).await.unwrap(),
        vec![refreshed.clone()]
    );
    publish(&agent, chat).await;
    publish(&agent, chat).await;
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
