mod support;

use std::{io::Cursor as IoCursor, path::Path, sync::Arc, time::Duration};

use async_trait::async_trait;
use sha2::{Digest, Sha256};
use tickloop_agent::{
    agent_access::*,
    image::{Image, MAX_BASE64_BYTES, prepare},
    model::*,
    runtime::{Agent, EventKind, ImageContext, ImageOrigin, InputContext, RewindMode},
};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    net::{UnixListener, UnixStream},
    sync::mpsc::UnboundedSender,
};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

const RESPONSE_TIMEOUT: Duration = Duration::from_secs(180);

struct ImageModel {
    image_digest: [u8; 32],
    image_counts: std::sync::Mutex<Vec<usize>>,
}

struct TextModel {
    prompt_digest: [u8; 32],
    prompt_counts: std::sync::Mutex<Vec<usize>>,
}

#[async_trait]
impl ModelClient for TextModel {
    async fn generate_title(
        &self,
        request: tickloop_agent::model::ModelRequest,
        _: tokio::sync::mpsc::UnboundedSender<tickloop_agent::model::ModelDelta>,
        _: tokio_util::sync::CancellationToken,
    ) -> Result<tickloop_agent::model::ModelOutput, tickloop_agent::model::ModelError> {
        Ok(support::title_output(request))
    }

    fn identity(&self) -> ModelIdentity {
        ModelIdentity {
            provider: ModelProvider::Api,
            profile: "fixture".into(),
            protocol: Protocol::Messages,
            requested_model: "fixture".into(),
        }
    }

    async fn complete(
        &self,
        request: ModelRequest,
        _: UnboundedSender<ModelDelta>,
        _: CancellationToken,
    ) -> Result<ModelOutput, ModelError> {
        let mut count = 0;
        for message in request
            .messages
            .iter()
            .filter(|message| message.role == Role::User)
        {
            for content in &message.content {
                if let Content::Text { text } = content
                    && text.starts_with("Original long prompt:")
                {
                    assert_eq!(
                        <[u8; 32]>::from(Sha256::digest(text.as_bytes())),
                        self.prompt_digest
                    );
                    count += 1;
                }
            }
        }
        self.prompt_counts.lock().unwrap().push(count);
        Ok(ModelOutput {
            private_source: None,
            content: support::text("The entire original prompt is present."),
            usage: Usage::default(),
            stop_reason: "end_turn".into(),
        })
    }
}

#[async_trait]
impl ModelClient for ImageModel {
    async fn generate_title(
        &self,
        request: tickloop_agent::model::ModelRequest,
        _: tokio::sync::mpsc::UnboundedSender<tickloop_agent::model::ModelDelta>,
        _: tokio_util::sync::CancellationToken,
    ) -> Result<tickloop_agent::model::ModelOutput, tickloop_agent::model::ModelError> {
        Ok(support::title_output(request))
    }

    fn identity(&self) -> ModelIdentity {
        ModelIdentity {
            provider: ModelProvider::Api,
            profile: "fixture".into(),
            protocol: Protocol::Messages,
            requested_model: "fixture".into(),
        }
    }
    async fn complete(
        &self,
        request: ModelRequest,
        _: UnboundedSender<ModelDelta>,
        _: CancellationToken,
    ) -> Result<ModelOutput, ModelError> {
        let mut count = 0;
        for content in request.messages.iter().flat_map(|message| &message.content) {
            if let Content::Image {
                source: ImageSource::Base64 { data, .. },
                ..
            } = content
            {
                assert_eq!(
                    <[u8; 32]>::from(Sha256::digest(data.as_bytes())),
                    self.image_digest
                );
                count += 1;
            }
        }
        self.image_counts.lock().unwrap().push(count);
        Ok(ModelOutput {
            private_source: None,
            content: support::text("The images remain readable."),
            usage: Usage::default(),
            stop_reason: "end_turn".into(),
        })
    }
}

fn legal_large_image() -> Image {
    let mut source = IoCursor::new(Vec::new());
    image::DynamicImage::new_rgb8(1, 1)
        .write_to(&mut source, image::ImageFormat::Png)
        .unwrap();
    let mut source = source.into_inner();
    source.resize(MAX_BASE64_BYTES / 4 * 3, 0);
    let prepared = prepare(&source).unwrap();
    assert_eq!(prepared.data.len(), MAX_BASE64_BYTES);
    prepared
}

fn image_contexts(image: &Image, count: u32) -> Vec<InputContext> {
    (1..=count)
        .map(|id| {
            InputContext::Image(ImageContext {
                origin: ImageOrigin::Pasted { id },
                image: image.clone(),
            })
        })
        .collect()
}

fn envelope(epoch: Uuid, command: AgentCommand) -> CommandEnvelope {
    CommandEnvelope {
        owner_epoch: epoch,
        command_id: Uuid::new_v4(),
        command,
    }
}

async fn complete(observation: &mut Observation) {
    tokio::time::timeout(Duration::from_secs(60), async {
        loop {
            if matches!(observation.recv().await.unwrap().change,
                ObservationChange::Event(event) if matches!(event.kind, EventKind::Completed { .. }))
            {
                break;
            }
        }
    }).await.unwrap();
}

async fn write_json<W: AsyncWrite + Unpin>(stream: &mut W, value: &serde_json::Value) {
    let bytes = serde_json::to_vec(value).unwrap();
    assert!(bytes.len() <= MAX_FRAME_BYTES);
    stream.write_u32(bytes.len() as u32).await.unwrap();
    stream.write_all(&bytes).await.unwrap();
}

async fn read_json<R: AsyncRead + Unpin>(stream: &mut R) -> serde_json::Value {
    tokio::time::timeout(RESPONSE_TIMEOUT, async {
        let length = stream.read_u32().await.unwrap() as usize;
        assert!(length <= MAX_FRAME_BYTES);
        let mut bytes = vec![0; length];
        stream.read_exact(&mut bytes).await.unwrap();
        serde_json::from_slice(&bytes).unwrap()
    })
    .await
    .expect("the fixture must return a complete frame")
}

enum ResponseFault {
    DisconnectAfterFirstChunk,
    InvalidDigest,
}

async fn fault_proxy(
    source: &Path,
    socket: &Path,
    fault: ResponseFault,
) -> tokio::task::JoinHandle<()> {
    let listener = UnixListener::bind(socket).unwrap();
    let source = source.to_path_buf();
    tokio::spawn(async move {
        let (client, _) = listener.accept().await.unwrap();
        let upstream = UnixStream::connect(source).await.unwrap();
        let (mut client_read, mut client_write) = client.into_split();
        let (mut upstream_read, mut upstream_write) = upstream.into_split();
        let requests = tokio::spawn(async move {
            let _ = tokio::io::copy(&mut client_read, &mut upstream_write).await;
        });
        loop {
            let mut response = read_json(&mut upstream_read).await;
            if response.get("ResponseEnd").is_some()
                && matches!(fault, ResponseFault::InvalidDigest)
            {
                response["ResponseEnd"]["sha256"] = "invalid digest".into();
                write_json(&mut client_write, &response).await;
                break;
            }
            let chunk = response.get("ResponseChunk").is_some();
            write_json(&mut client_write, &response).await;
            if chunk && matches!(fault, ResponseFault::DisconnectAfterFirstChunk) {
                break;
            }
        }
        requests.abort();
        let _ = requests.await;
    })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn repeated_legal_images_cross_the_frame_limit_without_losing_history_or_rewind_bytes() {
    let directory = tempfile::tempdir_in("/tmp").unwrap();
    let image = legal_large_image();
    let digest: [u8; 32] = Sha256::digest(image.data.as_bytes()).into();
    let model = Arc::new(ImageModel {
        image_digest: digest,
        image_counts: std::sync::Mutex::new(Vec::new()),
    });
    let runtime = Agent::new_embedded(support::config(), model.clone());
    let access = Arc::new(EmbeddedAgentAccess::new(runtime.clone()));
    let socket = directory.path().join("history.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    let epoch = access.owner_epoch();
    let server_access = access.clone();
    let server = tokio::spawn(async move {
        serve_listener(listener, server_access, epoch)
            .await
            .unwrap();
    });
    let client = LocalAgentClient::connect(&socket).await.unwrap();
    let result = client
        .execute(envelope(
            epoch,
            AgentCommand::Create {
                launch: LaunchSpec::embedded(directory.path().to_path_buf()),
                initial: None,
                origin: None,
            },
        ))
        .await
        .unwrap()
        .into_result()
        .unwrap();
    let CommandResult::Created { execution, .. } = result else {
        panic!("expected Create");
    };
    let mut feed = client
        .observe(ObservationRequest::Agent {
            execution,
            detail: AgentDetailQuery::Snapshot,
            after: None,
        })
        .await
        .unwrap();
    for prompt in [
        "Inspect the first fourteen images",
        "Inspect the next fourteen images",
    ] {
        client
            .execute(envelope(
                epoch,
                AgentCommand::Submit {
                    execution,
                    target_loop: None,
                    input: UserInput::Text {
                        text: prompt.into(),
                        contexts: image_contexts(&image, 14),
                        paths: Vec::new(),
                    },
                    origin: None,
                },
            ))
            .await
            .unwrap()
            .into_result()
            .unwrap();
        complete(&mut feed).await;
    }
    assert_eq!(*model.image_counts.lock().unwrap(), [14, 28]);
    let mut observation = tokio::time::timeout(
        RESPONSE_TIMEOUT,
        client.observe(ObservationRequest::Agent {
            execution,
            detail: AgentDetailQuery::Snapshot,
            after: None,
        }),
    )
    .await
    .unwrap()
    .expect("a legal full history must remain readable over IPC");
    let ObservationValue::Agent(snapshot) =
        std::mem::replace(&mut observation.initial, ObservationValue::Receipt(None))
    else {
        panic!("expected Snapshot");
    };
    assert_eq!(
        snapshot
            .conversation
            .message_contexts
            .values()
            .map(Vec::len)
            .sum::<usize>(),
        28
    );
    assert!(snapshot.conversation.messages.iter().flat_map(|message| &message.content).any(|content| matches!(content, Content::Text { text } if text == "The images remain readable.")));
    for context in snapshot.conversation.message_contexts.values().flatten() {
        let InputContext::Image(context) = context else {
            panic!("expected image context");
        };
        assert_eq!(
            <[u8; 32]>::from(Sha256::digest(context.image.data.as_bytes())),
            digest
        );
    }
    let second_prompt = snapshot
        .conversation
        .rewind_points()
        .last()
        .unwrap()
        .message_id;
    drop(snapshot);
    drop(observation);
    let proxy_socket = directory.path().join("lost-ack.sock");
    let proxy = fault_proxy(
        &socket,
        &proxy_socket,
        ResponseFault::DisconnectAfterFirstChunk,
    )
    .await;
    let interrupted = LocalAgentClient::connect(&proxy_socket).await.unwrap();
    let branch_command = envelope(
        epoch,
        AgentCommand::EditHistory {
            execution,
            action: HistoryAction::Branch {
                target_loop: None,
                name: Some("Retained images".into()),
            },
        },
    );
    let branch_id = branch_command.command_id;
    let error = tokio::time::timeout(RESPONSE_TIMEOUT, interrupted.execute(branch_command))
        .await
        .expect("a lost acknowledgement must resolve without hanging")
        .expect_err("a partial receipt must not be published");
    assert_eq!(error.code, AccessErrorCode::OutcomeUnknown);
    proxy.await.unwrap();
    let recovered = tokio::time::timeout(
        RESPONSE_TIMEOUT,
        client.observe(ObservationRequest::Receipt {
            epoch,
            command_id: branch_id,
        }),
    )
    .await
    .expect("the original receipt must remain queryable")
    .unwrap();
    let ObservationValue::Receipt(Some(receipt)) = recovered.initial else {
        panic!("the accepted CommandId must remain known");
    };
    assert_eq!(receipt.command_id, branch_id);
    assert!(matches!(receipt.state, ReceiptState::Completed));
    let CommandResult::Branch { snapshot, .. } = receipt.into_result().unwrap() else {
        panic!("expected original branch receipt");
    };
    assert_eq!(
        snapshot
            .conversation
            .message_contexts
            .values()
            .map(Vec::len)
            .sum::<usize>(),
        28
    );
    drop(snapshot);
    let directory_state = client
        .observe(ObservationRequest::Overview {
            filter: AgentFilter::default(),
            page: Page::default(),
            after: None,
        })
        .await
        .unwrap();
    let ObservationValue::Overview(entries) = directory_state.initial else {
        panic!("expected directory");
    };
    assert_eq!(
        entries.len(),
        2,
        "receipt reconciliation must not replay Branch"
    );
    assert_eq!(*model.image_counts.lock().unwrap(), [14, 28]);
    let receipt = tokio::time::timeout(
        RESPONSE_TIMEOUT,
        client.execute(envelope(
            epoch,
            AgentCommand::EditHistory {
                execution,
                action: HistoryAction::Rewind {
                    message_id: second_prompt,
                    mode: RewindMode::Conversation,
                },
            },
        )),
    )
    .await
    .expect("the original rewind draft must complete")
    .unwrap()
    .into_result()
    .unwrap();
    let CommandResult::Rewind {
        draft: Some(draft), ..
    } = receipt
    else {
        panic!("expected original rewind draft");
    };
    assert_eq!(draft.contexts.len(), 14);
    for context in draft.contexts {
        let InputContext::Image(context) = context else {
            panic!("expected image draft");
        };
        assert_eq!(
            <[u8; 32]>::from(Sha256::digest(context.image.data.as_bytes())),
            digest
        );
    }
    drop(feed);
    drop(client);
    server.abort();
    runtime.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn large_text_history_is_complete_and_a_corrupt_end_is_never_published() {
    let directory = tempfile::tempdir_in("/tmp").unwrap();
    let prompt = format!(
        "Original long prompt:{}\n\"Original ending\"",
        "中".repeat(68 * 1024 * 1024 / 3)
    );
    let digest: [u8; 32] = Sha256::digest(prompt.as_bytes()).into();
    let model = Arc::new(TextModel {
        prompt_digest: digest,
        prompt_counts: std::sync::Mutex::new(Vec::new()),
    });
    let runtime = Agent::new_embedded(support::config(), model.clone());
    let access = Arc::new(EmbeddedAgentAccess::new(runtime.clone()));
    let socket = directory.path().join("text.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    let epoch = access.owner_epoch();
    let server_access = access.clone();
    let server = tokio::spawn(async move {
        serve_listener(listener, server_access, epoch)
            .await
            .unwrap();
    });
    let client = LocalAgentClient::connect(&socket).await.unwrap();
    let created = client
        .execute(envelope(
            epoch,
            AgentCommand::Create {
                launch: LaunchSpec::embedded(directory.path().to_path_buf()),
                initial: None,
                origin: None,
            },
        ))
        .await
        .unwrap()
        .into_result()
        .unwrap();
    let CommandResult::Created { execution, .. } = created else {
        panic!("expected Create");
    };
    client
        .execute(envelope(
            epoch,
            AgentCommand::Configure {
                provider: None,
                execution,
                change: ConfigurationChange::ContextWindow {
                    tokens: 100_000_000,
                },
            },
        ))
        .await
        .unwrap()
        .into_result()
        .unwrap();
    let mut feed = client
        .observe(ObservationRequest::Agent {
            execution,
            detail: AgentDetailQuery::Snapshot,
            after: None,
        })
        .await
        .unwrap();
    for _ in 0..2 {
        client
            .execute(envelope(
                epoch,
                AgentCommand::Submit {
                    execution,
                    target_loop: None,
                    input: UserInput::Text {
                        text: prompt.clone(),
                        contexts: Vec::new(),
                        paths: Vec::new(),
                    },
                    origin: None,
                },
            ))
            .await
            .unwrap()
            .into_result()
            .unwrap();
        complete(&mut feed).await;
    }
    assert_eq!(*model.prompt_counts.lock().unwrap(), [1, 2]);
    let proxy_socket = directory.path().join("corrupt.sock");
    let proxy = fault_proxy(&socket, &proxy_socket, ResponseFault::InvalidDigest).await;
    let corrupt = LocalAgentClient::connect(&proxy_socket).await.unwrap();
    let result = corrupt
        .observe(ObservationRequest::Agent {
            execution,
            detail: AgentDetailQuery::Snapshot,
            after: None,
        })
        .await;
    assert!(
        matches!(
            result,
            Err(AccessError {
                code: AccessErrorCode::Unavailable,
                ..
            })
        ),
        "no partial or corrupt snapshot may be published"
    );
    proxy.await.unwrap();
    let restored = tokio::time::timeout(
        RESPONSE_TIMEOUT,
        client.observe(ObservationRequest::Agent {
            execution,
            detail: AgentDetailQuery::Snapshot,
            after: None,
        }),
    )
    .await
    .expect("a full original snapshot must remain readable")
    .unwrap();
    let ObservationValue::Agent(snapshot) = restored.initial else {
        panic!("expected original snapshot");
    };
    let prompts = snapshot
        .conversation
        .messages
        .iter()
        .filter(|message| message.role == Role::User)
        .flat_map(|message| &message.content)
        .filter_map(|content| match content {
            Content::Text { text } if text.starts_with("Original long prompt:") => Some(text),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(prompts.len(), 2);
    for original in prompts {
        assert_eq!(original.len(), prompt.len());
        assert_eq!(
            <[u8; 32]>::from(Sha256::digest(original.as_bytes())),
            digest
        );
    }
    assert_eq!(
        *model.prompt_counts.lock().unwrap(),
        [1, 2],
        "readonly reconnect must not run the model"
    );
    drop(snapshot);
    drop(feed);
    drop(client);
    drop(corrupt);
    server.abort();
    runtime.shutdown().await;
}

#[tokio::test]
async fn a_response_with_a_gap_or_an_incomplete_end_never_becomes_an_observation() {
    for incomplete_end in [false, true] {
        let temporary = tempfile::tempdir_in("/tmp").unwrap();
        let socket = temporary.path().join("invalid.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let epoch = Uuid::new_v4();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            assert!(read_json(&mut stream).await.get("Hello").is_some());
            write_json(&mut stream, &serde_json::json!({"Hello": {
                "process_id": std::process::id(), "major": PROTOCOL_MAJOR, "minor": PROTOCOL_MINOR,
                "owner_epoch": epoch, "session_id": Uuid::new_v4(), "capabilities": ["ChunkedResponses"]
            }})).await;
            let request = read_json(&mut stream).await;
            let transfer = Uuid::new_v4();
            write_json(&mut stream, &serde_json::json!({"ResponseStart": {
                "request_id": request["Observe"]["request_id"], "transfer_id": transfer,
                "kind": "Opened", "total_bytes": MAX_FRAME_BYTES + 1, "chunk_bytes": RESPONSE_CHUNK_BYTES
            }})).await;
            let response = if incomplete_end {
                serde_json::json!({"ResponseEnd": {"transfer_id": transfer, "total_bytes": MAX_FRAME_BYTES + 1, "sha256": "empty"}})
            } else {
                serde_json::json!({"ResponseChunk": {"transfer_id": transfer, "offset": 1, "base64": "eA=="}})
            };
            write_json(&mut stream, &response).await;
        });
        let client = LocalAgentClient::connect(socket).await.unwrap();
        let result = tokio::time::timeout(
            Duration::from_secs(5),
            client.observe(ObservationRequest::Agent {
                execution: ExecutionRef {
                    owner_epoch: epoch,
                    agent_id: Uuid::new_v4(),
                    generation: 1,
                },
                detail: AgentDetailQuery::Snapshot,
                after: None,
            }),
        )
        .await
        .unwrap();
        assert!(matches!(
            result,
            Err(AccessError {
                code: AccessErrorCode::Unavailable,
                ..
            })
        ));
        assert!(!client.is_connected());
        server.await.unwrap();
    }
}
