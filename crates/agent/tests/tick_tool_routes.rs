mod support;

use serde_json::json;
use std::sync::Arc;
use tickloop_agent::{host_tools::HostToolProvider, native_host::NativeHostToolProvider};
use tokio_util::sync::CancellationToken;

#[tokio::test]
async fn native_tick_tools_redirect_artifact_and_shell_ids_before_identity_or_host_resolution() {
    let directory = tempfile::tempdir().unwrap();
    let provider = NativeHostToolProvider::new(Arc::new(tick_connect::access::TickAccess::new(
        tick_connect::herdr::HerdrContext::without_view(directory.path().join("unused.sock")),
    )));
    for (instance, native_tool) in [
        ("artifact:doc", "mcp__artifacts__artifact_read"),
        (
            "artifact:https://docs.example/doc",
            "mcp__artifacts__artifact_read",
        ),
        ("shell:task", "TaskStop"),
    ] {
        for name in ["ReadTick", "CallTickTool"] {
            let input = if name == "ReadTick" {
                json!({"tick_instance_id":instance})
            } else {
                json!({"tick_instance_id":instance,"tool":"read","args":{}})
            };
            let error = provider
                .prepare_call(uuid::Uuid::new_v4(), name, &input, CancellationToken::new())
                .await
                .unwrap_err();
            assert!(error.contains("NativeToolRequired"), "{error}");
            assert!(error.contains(native_tool), "{error}");
        }
    }
}

#[tokio::test]
async fn peers_with_the_old_delegation_authority_schema_are_rejected_at_handshake() {
    use tickloop_agent::agent_access::{AccessErrorCode, LocalAgentClient};
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::UnixListener,
    };
    let directory = tempfile::tempdir_in("/tmp").unwrap();
    let socket = directory.path().join("old-peer.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let length = stream.read_u32().await.unwrap();
        let mut request = vec![0; length as usize];
        stream.read_exact(&mut request).await.unwrap();
        let response = serde_json::to_vec(&json!({"Hello":{"major":3,"minor":2,"owner_epoch":uuid::Uuid::new_v4(),"session_id":uuid::Uuid::new_v4()}})).unwrap();
        stream.write_u32(response.len() as u32).await.unwrap();
        stream.write_all(&response).await.unwrap();
    });
    let result = LocalAgentClient::connect(&socket).await;
    server.await.unwrap();
    assert!(
        matches!(result, Err(ref error) if error.code == AccessErrorCode::VersionMismatch),
        "old peer accepted an incompatible delegation authority schema"
    );
}

#[tokio::test]
async fn root_native_redirect_precedes_invalid_host_authority_resolution() {
    use tickloop_agent::runtime::{Agent, EventKind};
    let directory = tempfile::tempdir().unwrap();
    let endpoint = directory.path().join("not-a-socket");
    std::fs::write(&endpoint, "fixture").unwrap();
    let provider = Arc::new(NativeHostToolProvider::new(Arc::new(
        tick_connect::access::TickAccess::new(tick_connect::herdr::HerdrContext::without_view(
            endpoint,
        )),
    )));
    let model = Arc::new(support::ScriptedModel::new(vec![
        support::call(
            "redirect",
            "CallTickTool",
            json!({"tick_instance_id":"artifact:doc","tool":"read","args":{}}),
        ),
        support::text("Done."),
    ]));
    let agent = Agent::new_embedded(support::config(), model);
    agent.install_host_tools(provider).unwrap();
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    let mut events = agent.subscribe();
    agent
        .submit(chat, None, "Inspect the artifact".into())
        .await
        .unwrap();
    let result = loop {
        if let EventKind::ToolCompleted { result, .. } = support::next_event(&mut events).await.kind
        {
            break result;
        }
    };
    agent.shutdown().await;
    assert!(result.is_error);
    assert!(
        result.content.contains("NativeToolRequired"),
        "{}",
        result.content
    );
}
