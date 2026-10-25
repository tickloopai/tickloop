use tickloop_harness_model::{
    LocalModelClients, ModelClients, ModelRequest, credentials::save_api_key,
};
use tickloop_harness_protocol::{
    Message, ModelAuthSource, ModelConfig, ModelProvider, Protocol, Role,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

fn root(directory: &tempfile::TempDir) -> std::path::PathBuf {
    directory.path().canonicalize().unwrap().join("state")
}

#[tokio::test]
async fn model_factory_reads_saved_key_for_each_request() {
    let directory = tempfile::tempdir().unwrap();
    let root = root(&directory);
    let key_name = format!("TICKLOOP_TEST_KEY_{}", uuid::Uuid::new_v4().simple()).to_uppercase();
    save_api_key(&root, &key_name, "fake-first-key").unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base_url = format!("http://{}/anthropic", listener.local_addr().unwrap());
    let (request_sender, mut received_requests) = mpsc::unbounded_channel();
    let server = tokio::spawn(async move {
        for _ in 0..2 {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut bytes = Vec::new();
            while !bytes.windows(4).any(|window| window == b"\r\n\r\n") {
                let mut buffer = [0; 4096];
                let length = socket.read(&mut buffer).await.unwrap();
                assert_ne!(length, 0);
                bytes.extend_from_slice(&buffer[..length]);
            }
            request_sender
                .send(String::from_utf8(bytes).unwrap())
                .unwrap();
            socket.write_all(b"HTTP/1.1 401 Unauthorized\r\nContent-Type: text/event-stream\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}").await.unwrap();
        }
    });
    let config = ModelConfig {
        name: "fixture".into(),
        protocol: Protocol::Messages,
        base_url,
        model: "fixture-model".into(),
        api_key_env: key_name.clone(),
        max_output_tokens: 32,
        provider: ModelProvider::Api,
        auth_source: ModelAuthSource::ApiKeyEnv,
        provider_id: String::new(),
        provider_name: String::new(),
        model_id: String::new(),
        connection: None,
        card: None,
        input_modalities: vec![tickloop_harness_protocol::InputModality::Text],
    };
    let client = LocalModelClients::new(Some(root.to_path_buf()))
        .client(&config)
        .unwrap();
    let request = || ModelRequest {
        history_sources: Vec::new(),
        messages: vec![Message {
            role: Role::User,
            content: vec![],
        }],
        tools: vec![],
        structured_output: None,
        temperature: None,
        reasoning_effort: None,
    };
    let (events, _) = mpsc::unbounded_channel();
    assert!(
        client
            .complete(request(), events, CancellationToken::new())
            .await
            .is_err()
    );
    let first = received_requests.recv().await.unwrap();
    assert!(first.contains("x-api-key: fake-first-key"));

    save_api_key(&root, &key_name, "fake-second-key").unwrap();
    let (events, _) = mpsc::unbounded_channel();
    assert!(
        client
            .complete(request(), events, CancellationToken::new())
            .await
            .is_err()
    );
    let second = received_requests.recv().await.unwrap();
    assert!(second.contains("x-api-key: fake-second-key"));
    server.await.unwrap();
}
