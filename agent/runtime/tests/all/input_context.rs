use std::{path::Path, sync::Arc};

use base64::{Engine, engine::general_purpose::STANDARD as BASE64};
use image::{DynamicImage, ImageFormat, Rgb, RgbImage};
use tickloop_agent_base::image::prepare;
use tickloop_agent_model::ModelRequest;
use tickloop_agent_protocol::{
    BranchRequest, ChatId, Content, Decision, EventKind, ImageContext, ImageMediaType, ImageOrigin,
    ImageSource, InputContext, InputModality, Message, ModelChange, ModelConfig, PermissionRule,
    RewindMode,
};
use tickloop_agent_runtime::{Agent, AgentError};
use uuid::Uuid;

use tickloop_agent_test_support::harness::{ChosenModels, embedded_agent};
use tickloop_agent_test_support::{ScriptedModel, call, config, next_event, text};

fn write_png(path: &Path, width: u32, height: u32) -> Vec<u8> {
    let mut seed = width ^ (height << 16) ^ 0x9e37_79b9;
    let pixels = RgbImage::from_fn(width, height, |_, _| {
        seed ^= seed << 13;
        seed ^= seed >> 17;
        seed ^= seed << 5;
        let [red, green, blue, _] = seed.to_le_bytes();
        Rgb([red, green, blue])
    });
    let mut bytes = Vec::new();
    DynamicImage::ImageRgb8(pixels)
        .write_to(&mut std::io::Cursor::new(&mut bytes), ImageFormat::Png)
        .unwrap();
    std::fs::write(path, &bytes).unwrap();
    bytes
}

async fn submit_and_wait(
    agent: &Agent,
    chat: ChatId,
    prompt: &str,
    paths: Vec<std::path::PathBuf>,
) {
    submit_contexts_and_wait(agent, chat, prompt, Vec::new(), paths).await;
}

async fn submit_contexts_and_wait(
    agent: &Agent,
    chat: ChatId,
    prompt: &str,
    contexts: Vec<InputContext>,
    paths: Vec<std::path::PathBuf>,
) {
    let mut events = agent.subscribe();
    let receipt = agent
        .submit_with_input_contexts(chat, None, prompt.into(), contexts, paths)
        .await
        .unwrap();
    loop {
        let event = next_event(&mut events).await;
        if event.run_id == receipt.run_id && matches!(event.kind, EventKind::Completed { .. }) {
            return;
        }
    }
}

fn pasted_image(id: u32, bytes: &[u8]) -> InputContext {
    InputContext::Image(ImageContext {
        origin: ImageOrigin::Pasted { id },
        image: prepare(bytes).unwrap(),
    })
}

/// The label and image block that follow each attached image in a model request.
fn requested_images(request: &ModelRequest) -> Vec<(String, u32, u32, String)> {
    request
        .messages
        .iter()
        .flat_map(|message| message.content.windows(2))
        .filter_map(|pair| match pair {
            [
                Content::Text { text },
                Content::Image {
                    source: ImageSource::Base64 { media_type, data },
                    width,
                    height,
                },
            ] => {
                assert_eq!(*media_type, ImageMediaType::Png);
                Some((text.clone(), *width, *height, data.clone()))
            }
            _ => None,
        })
        .collect()
}

fn is_text_only(message: &Message) -> bool {
    message
        .content
        .iter()
        .all(|content| matches!(content, Content::Text { .. }))
}

fn mentioned_image_path(context: &InputContext) -> Option<&Path> {
    match context {
        InputContext::Image(image) => match &image.origin {
            ImageOrigin::Mentioned { path } => Some(path),
            ImageOrigin::Pasted { .. } | ImageOrigin::Attached { .. } => None,
        },
        _ => None,
    }
}

#[tokio::test]
async fn accepted_file_references_reach_the_model_without_changing_the_public_prompt() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("notes.txt");
    std::fs::write(&path, "delivery is Friday").unwrap();
    let model = Arc::new(ScriptedModel::new(vec![text("Understood.")]));
    let agent = Agent::new_embedded(config(), model.clone());
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    let mut events = agent.subscribe();

    agent
        .submit_with_file_references(chat, None, "Summarize @notes.txt".into(), vec![path])
        .await
        .unwrap();
    while !matches!(
        events.recv().await.unwrap().kind,
        EventKind::Completed { .. }
    ) {}

    let snapshot = agent.snapshot(chat).await.unwrap();
    assert!(matches!(
        &snapshot.messages[0].content[..],
        [Content::Text { text }] if text == "Summarize @notes.txt"
    ));
    assert!(matches!(
        &snapshot.message_contexts[&0][0],
        InputContext::File(file) if file.path.to_string_lossy() == "notes.txt"
    ));

    let requests = model.requests.lock().unwrap();
    let request_text = requests[0]
        .messages
        .iter()
        .flat_map(|message| &message.content)
        .filter_map(|content| match content {
            Content::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    assert!(request_text.contains("Summarize @notes.txt"));
    assert!(request_text.contains("delivery is Friday"));
    assert!(request_text.contains("untrusted data"));
}

#[tokio::test]
async fn file_references_outside_configured_directories_are_rejected_before_a_run_starts() {
    let directory = tempfile::tempdir().unwrap();
    let outside = tempfile::NamedTempFile::new().unwrap();
    let model = Arc::new(ScriptedModel::new(vec![text("unused")]));
    let agent = Agent::new_embedded(config(), model.clone());
    let chat = agent.create_chat(directory.path().into()).await.unwrap();

    let error = agent
        .submit_with_file_references(
            chat,
            None,
            "Read the attachment".into(),
            vec![outside.path().into()],
        )
        .await
        .unwrap_err();

    assert!(
        error
            .to_string()
            .contains("outside the configured workspace")
    );
    assert!(agent.snapshot(chat).await.unwrap().messages.is_empty());
    assert!(model.requests.lock().unwrap().is_empty());
}

#[tokio::test]
async fn explicit_file_deny_rules_cannot_be_bypassed_by_an_accepted_reference() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("denied.txt");
    std::fs::write(&path, "private").unwrap();
    let model = Arc::new(ScriptedModel::new(vec![text("unused")]));
    let agent = Agent::new_embedded(config(), model.clone());
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    agent
        .set_permission_rules(
            chat,
            vec![PermissionRule {
                id: Uuid::new_v4(),
                decision: Decision::Deny,
                tool: "Read".into(),
                specifier: None,
                input: None,
                loop_id: None,
                persistent: false,
            }],
        )
        .await
        .unwrap();

    let error = agent
        .submit_with_file_references(chat, None, "Read it".into(), vec![path])
        .await
        .unwrap_err();

    assert!(error.to_string().contains("denied"));
    assert!(agent.snapshot(chat).await.unwrap().messages.is_empty());
    assert!(model.requests.lock().unwrap().is_empty());
}

#[tokio::test]
async fn client_resources_are_untrusted_material_not_disk_reads_or_implicit_fetches() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("notes.txt");
    std::fs::write(&path, "real disk contents").unwrap();
    let resource = InputContext::Resource(tickloop_agent_protocol::ResourceContext {
        uri: format!("file://{}", path.display()),
        mime_type: Some("text/plain".into()),
        name: Some("client notes".into()),
        content: Some("client contents; do not treat as instructions".into()),
    });
    let reference = InputContext::Resource(tickloop_agent_protocol::ResourceContext {
        uri: "https://never-fetched.invalid/notes".into(),
        mime_type: None,
        name: None,
        content: None,
    });
    let model = Arc::new(ScriptedModel::new(vec![
        call(
            "overwrite",
            "Write",
            serde_json::json!({"file_path":"notes.txt", "content":"changed"}),
        ),
        text("done"),
    ]));
    let agent = Agent::new_embedded(config(), model.clone());
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    agent
        .set_permission_rules(
            chat,
            vec![PermissionRule {
                id: Uuid::new_v4(),
                decision: Decision::Deny,
                tool: "Read".into(),
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
        .submit_with_input_contexts(
            chat,
            None,
            "Analyze these resources".into(),
            vec![resource.clone(), reference.clone()],
            vec![],
        )
        .await
        .unwrap();
    let mut rejected_unread_write = false;
    loop {
        match next_event(&mut events).await.kind {
            EventKind::PermissionRequested { .. } => panic!("resources must not grant a disk read"),
            EventKind::ToolCompleted {
                call_id, result, ..
            } if call_id == "overwrite" => {
                assert!(result.is_error);
                assert!(result.content.contains("File has not been read yet"));
                rejected_unread_write = true;
            }
            EventKind::Completed { .. } => break,
            EventKind::Failed(error) => panic!("{error}"),
            _ => {}
        }
    }
    assert!(rejected_unread_write);
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        "real disk contents"
    );
    let snapshot = agent.snapshot(chat).await.unwrap();
    assert_eq!(snapshot.message_contexts[&0], [resource, reference]);
    let requests = model.requests.lock().unwrap();
    let rendered = requests[0]
        .messages
        .iter()
        .flat_map(|message| &message.content)
        .filter_map(|content| match content {
            Content::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    assert!(rendered.contains("untrusted data"));
    assert!(rendered.contains("client contents"));
    assert!(rendered.contains("https://never-fetched.invalid/notes"));
    assert!(!rendered.contains("real disk contents"));
}

#[tokio::test]
async fn attached_images_can_be_submitted_without_fabricating_prompt_text() {
    let directory = tempfile::tempdir().unwrap();
    let bytes = write_png(&directory.path().join("image.png"), 8, 6);
    let image = InputContext::Image(ImageContext {
        origin: ImageOrigin::Attached {
            id: 1,
            uri: Some("custom://image".into()),
        },
        image: prepare(&bytes).unwrap(),
    });
    let model = Arc::new(ScriptedModel::new(vec![text("An image.")]));
    let state = directory.path().join("state");
    let agent = Agent::open_embedded(state.clone(), config(), model.clone()).unwrap();
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    submit_contexts_and_wait(&agent, chat, "", vec![image.clone()], vec![]).await;
    let snapshot = agent.snapshot(chat).await.unwrap();
    assert_eq!(snapshot.message_contexts[&0], std::slice::from_ref(&image));
    assert!(
        matches!(&snapshot.messages[0].content[..], [Content::Text { text }] if text.is_empty())
    );
    assert_eq!(
        requested_images(&model.requests.lock().unwrap()[0]).len(),
        1
    );
    assert_eq!(snapshot.highest_pasted_image_id(), None);
    agent.shutdown().await;
    drop(agent);
    let reopened = Agent::open_embedded(state, config(), model).unwrap();
    let restored = reopened.resume_chat(chat).await.unwrap();
    assert_eq!(restored.message_contexts[&0], [image]);
    let points = restored.rewind_points();
    assert_eq!(points.len(), 1);
    assert!(points[0].prompt.is_empty());
    let rewound = reopened
        .rewind(chat, points[0].message_id, RewindMode::Conversation)
        .await
        .unwrap();
    let draft = rewound.draft.unwrap();
    assert!(draft.prompt.is_empty());
    assert_eq!(draft.contexts, restored.message_contexts[&0]);
    reopened.shutdown().await;
}

#[tokio::test]
async fn mentioned_images_reach_the_model_as_a_label_and_an_image_block() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("chart.png");
    let bytes = write_png(&path, 64, 48);
    let model = Arc::new(ScriptedModel::new(vec![text("A noisy chart.")]));
    let agent = Agent::new_embedded(config(), model.clone());
    let chat = agent.create_chat(directory.path().into()).await.unwrap();

    submit_and_wait(&agent, chat, "Describe @chart.png", vec![path]).await;

    let snapshot = agent.snapshot(chat).await.unwrap();
    assert!(matches!(
        &snapshot.messages[0].content[..],
        [Content::Text { text }] if text == "Describe @chart.png"
    ));
    assert_eq!(
        mentioned_image_path(&snapshot.message_contexts[&0][0]),
        Some(Path::new("chart.png"))
    );
    let requests = model.requests.lock().unwrap();
    assert_eq!(
        requested_images(&requests[0]),
        vec![("chart.png".into(), 64, 48, BASE64.encode(&bytes))]
    );
    assert!(
        requests[0]
            .messages
            .iter()
            .any(|message| matches!(&message.content[0], Content::Text { text } if text == "Describe @chart.png"))
    );
}

#[tokio::test]
async fn unsupported_image_formats_are_rejected_with_the_reason_before_a_run_starts() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("scan.bmp");
    std::fs::write(&path, b"BM\x46\x00\x00\x00\x00\x00\x00\x00\x36\x00").unwrap();
    let model = Arc::new(ScriptedModel::new(vec![text("unused")]));
    let agent = Agent::new_embedded(config(), model.clone());
    let chat = agent.create_chat(directory.path().into()).await.unwrap();

    let error = agent
        .submit_with_file_references(chat, None, "Read @scan.bmp".into(), vec![path])
        .await
        .unwrap_err()
        .to_string();

    assert!(error.contains("scan.bmp"), "{error}");
    assert!(error.contains("unsupported image format"), "{error}");
    assert!(agent.snapshot(chat).await.unwrap().messages.is_empty());
    assert!(model.requests.lock().unwrap().is_empty());
}

#[tokio::test]
async fn image_references_follow_the_file_permission_rules() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("private.png");
    write_png(&path, 8, 8);
    let model = Arc::new(ScriptedModel::new(vec![text("unused")]));
    let agent = Agent::new_embedded(config(), model.clone());
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    agent
        .set_permission_rules(
            chat,
            vec![PermissionRule {
                id: Uuid::new_v4(),
                decision: Decision::Deny,
                tool: "Read".into(),
                specifier: None,
                input: None,
                loop_id: None,
                persistent: false,
            }],
        )
        .await
        .unwrap();

    let error = agent
        .submit_with_file_references(chat, None, "Look @private.png".into(), vec![path])
        .await
        .unwrap_err();

    assert!(error.to_string().contains("denied"));
    assert!(model.requests.lock().unwrap().is_empty());
}

#[tokio::test]
async fn attached_images_survive_reopening_rewind_and_branching() {
    let directory = tempfile::tempdir().unwrap();
    let state = directory.path().join("state");
    let path = directory.path().join("chart.png");
    let bytes = write_png(&path, 32, 16);
    let expected = vec![("chart.png".to_string(), 32, 16, BASE64.encode(&bytes))];
    let agent = Agent::open_embedded(
        state.clone(),
        config(),
        Arc::new(ScriptedModel::new(vec![text("seen")])),
    )
    .unwrap();
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    submit_and_wait(&agent, chat, "Describe @chart.png", vec![path.clone()]).await;
    agent.shutdown().await;
    drop(agent);
    std::fs::remove_file(&path).unwrap();

    let model = Arc::new(ScriptedModel::new(vec![
        text("still seen"),
        text("branch sees it"),
    ]));
    let reopened = Agent::open_embedded(state, config(), model.clone()).unwrap();
    let restored = reopened.resume_chat(chat).await.unwrap();
    assert!(restored.messages.iter().all(is_text_only));
    assert_eq!(
        mentioned_image_path(&restored.message_contexts[&0][0]),
        Some(Path::new("chart.png"))
    );
    submit_and_wait(&reopened, chat, "And now?", Vec::new()).await;
    assert_eq!(
        requested_images(&model.requests.lock().unwrap()[0]),
        expected
    );

    let branch = reopened
        .branch_chat(BranchRequest {
            request_id: Uuid::new_v4(),
            source_chat_id: chat,
            source_loop_id: None,
            name: None,
        })
        .await
        .unwrap();
    submit_and_wait(
        &reopened,
        branch.child_chat_id,
        "Branch question",
        Vec::new(),
    )
    .await;
    assert_eq!(
        requested_images(&model.requests.lock().unwrap()[1]),
        expected
    );

    let point = reopened.snapshot(chat).await.unwrap().rewind_points()[0].clone();
    let rewound = reopened
        .rewind(chat, point.message_id, RewindMode::Conversation)
        .await
        .unwrap();
    let draft = rewound.draft.unwrap();
    assert_eq!(draft.prompt, "Describe @chart.png");
    assert_eq!(draft.contexts, restored.message_contexts[&0]);
    reopened.shutdown().await;
}

#[tokio::test]
async fn pasted_images_reach_the_model_in_placeholder_order_and_their_numbers_survive_reopening() {
    let directory = tempfile::tempdir().unwrap();
    let state = directory.path().join("state");
    let wide = write_png(&directory.path().join("wide.png"), 24, 12);
    let tall = write_png(&directory.path().join("tall.png"), 10, 20);
    let model = Arc::new(ScriptedModel::new(vec![text("Compared.")]));
    let agent = Agent::open_embedded(state.clone(), config(), model.clone()).unwrap();
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    let prompt = "Compare [Image #3] with [Image #2]";

    submit_contexts_and_wait(
        &agent,
        chat,
        prompt,
        vec![pasted_image(3, &tall), pasted_image(2, &wide)],
        Vec::new(),
    )
    .await;

    assert_eq!(
        requested_images(&model.requests.lock().unwrap()[0]),
        vec![
            ("[Image #3]".into(), 10, 20, BASE64.encode(&tall)),
            ("[Image #2]".into(), 24, 12, BASE64.encode(&wide)),
        ]
    );
    let snapshot = agent.snapshot(chat).await.unwrap();
    assert!(matches!(
        &snapshot.messages[0].content[..],
        [Content::Text { text }] if text == prompt
    ));
    assert_eq!(snapshot.highest_pasted_image_id(), Some(3));
    agent.shutdown().await;
    drop(agent);

    let reopened = Agent::open_embedded(state, config(), model).unwrap();
    let restored = reopened.resume_chat(chat).await.unwrap();
    assert_eq!(restored.highest_pasted_image_id(), Some(3));
    reopened.shutdown().await;
}

#[tokio::test]
async fn images_count_by_pixels_so_a_large_payload_does_not_force_compaction() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("screenshot.png");
    let bytes = write_png(&path, 400, 400);
    assert!(BASE64.encode(&bytes).len() > 400_000);
    let model = Arc::new(ScriptedModel::new(vec![text("Looked at it.")]));
    let agent = Agent::open_embedded(
        directory.path().join("state"),
        tickloop_agent_test_support::config_with_window(100_000),
        model.clone(),
    )
    .unwrap();
    let chat = agent.create_chat(directory.path().into()).await.unwrap();

    submit_and_wait(&agent, chat, "Check @screenshot.png", vec![path]).await;

    let requests = model.requests.lock().unwrap().clone();
    assert_eq!(requests.len(), 1);
    assert_eq!(requested_images(&requests[0]).len(), 1);
    agent.shutdown().await;
}

fn text_only_config() -> ModelConfig {
    ModelConfig {
        name: "text only".into(),
        input_modalities: vec![InputModality::Text],
        ..config()
    }
}

#[tokio::test]
async fn a_model_without_image_input_refuses_image_prompts_and_leaves_the_chat_unchanged() {
    let directory = tempfile::tempdir().unwrap();
    let image = directory.path().join("chart.png");
    let bytes = write_png(&image, 16, 16);
    let notes = directory.path().join("notes.txt");
    std::fs::write(&notes, "delivery is Friday").unwrap();
    let model = Arc::new(ScriptedModel::new(vec![text("Read the notes.")]));
    let models = ChosenModels::new(model.clone());
    let agent = embedded_agent(text_only_config(), models.clone());
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    assert!(!agent.accepts_images(chat).await.unwrap());

    let error = agent
        .submit_with_file_references(
            chat,
            None,
            "Describe @chart.png".into(),
            vec![image.clone()],
        )
        .await
        .unwrap_err();

    assert!(matches!(error, AgentError::ImageInputUnsupported));
    let message = error.to_string();
    assert!(
        message.contains("does not support image input"),
        "{message}"
    );
    assert!(message.contains("@image"), "{message}");
    assert!(message.contains("switch"), "{message}");
    let snapshot = agent.snapshot(chat).await.unwrap();
    assert!(snapshot.messages.is_empty());
    assert!(snapshot.loops.is_empty());
    assert!(snapshot.pending_inputs.is_empty());
    assert!(model.requests.lock().unwrap().is_empty());

    submit_and_wait(&agent, chat, "Summarize @notes.txt", vec![notes]).await;
    assert_eq!(model.requests.lock().unwrap().len(), 1);

    let vision = Arc::new(ScriptedModel::new(vec![text("A chart.")]));
    models.choose(vision.clone());
    agent.set_model(chat, config()).await.unwrap();
    assert!(agent.accepts_images(chat).await.unwrap());
    submit_and_wait(&agent, chat, "Describe @chart.png", vec![image]).await;
    assert_eq!(
        requested_images(&vision.requests.lock().unwrap()[0]),
        vec![("chart.png".into(), 16, 16, BASE64.encode(&bytes))]
    );
}

#[tokio::test]
async fn a_pending_switch_to_a_text_only_model_refuses_image_prompts() {
    let directory = tempfile::tempdir().unwrap();
    let image = directory.path().join("chart.png");
    write_png(&image, 8, 8);
    let model = Arc::new(ScriptedModel::new(vec![call(
        "write-1",
        "Write",
        serde_json::json!({"file_path":"out.txt","content":"x"}),
    )]));
    let agent = Agent::new_embedded(config(), model);
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    agent
        .set_permission_rules(
            chat,
            vec![PermissionRule {
                id: Uuid::new_v4(),
                decision: Decision::Ask,
                tool: "Write".into(),
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
        .submit(chat, None, "Write the file".into())
        .await
        .unwrap();
    while !matches!(
        next_event(&mut events).await.kind,
        EventKind::PermissionRequested { .. }
    ) {}

    let change = agent.set_model(chat, text_only_config()).await.unwrap();

    assert_eq!(change, ModelChange::Pending);
    assert!(!agent.accepts_images(chat).await.unwrap());
    let error = agent
        .submit_with_file_references(chat, None, "Describe @chart.png".into(), vec![image])
        .await
        .unwrap_err();
    assert!(matches!(error, AgentError::ImageInputUnsupported));
}
