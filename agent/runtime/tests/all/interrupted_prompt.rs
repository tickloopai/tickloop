//! Claude Code's auto-restore on interrupt: a turn interrupted before any assistant text or tool
//! call gives its prompt back, and the model never sees that prompt again.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use image::{DynamicImage, ImageFormat, Rgb, RgbImage};
use tickloop_agent_base::image::prepare;
use tickloop_agent_model::{ModelClient, ModelDelta, ModelError, ModelOutput, ModelRequest};
use tickloop_agent_protocol::local_messages::INTERRUPT_MESSAGE;
use tickloop_agent_protocol::{
    ChatId, Content, Decision, Event, EventKind, ImageContext, ImageOrigin, InputContext, Message,
    PermissionRule, Receipt,
};
use tickloop_agent_runtime::Agent;
use tickloop_agent_test_support::{call, config, next_event, text, title_output};
use tokio::sync::broadcast::Receiver;
use tokio::sync::{Notify, mpsc::UnboundedSender};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

enum Step {
    /// Waits for the run to be interrupted without producing anything.
    Block,
    Reply(Vec<Content>),
}

/// Answers each model request with the next step; a `Block` holds the turn until it is
/// interrupted.
struct StepModel {
    steps: Mutex<VecDeque<Step>>,
    requests: Mutex<Vec<ModelRequest>>,
    entered: Notify,
}

impl StepModel {
    fn new(steps: Vec<Step>) -> Arc<Self> {
        Arc::new(Self {
            steps: Mutex::new(steps.into()),
            requests: Mutex::new(Vec::new()),
            entered: Notify::new(),
        })
    }

    /// Every text the last model request carried, the host state with its Loop goals included.
    fn last_request_text(&self) -> String {
        let requests = self.requests.lock().unwrap();
        message_text(&requests.last().expect("a model request").messages)
    }
}

#[async_trait]
impl ModelClient for StepModel {
    async fn generate_title(
        &self,
        request: ModelRequest,
        _: UnboundedSender<ModelDelta>,
        _: CancellationToken,
    ) -> Result<ModelOutput, ModelError> {
        Ok(title_output(request))
    }

    fn identity(&self) -> tickloop_agent_protocol::ModelIdentity {
        tickloop_agent_protocol::ModelIdentity {
            provider: tickloop_agent_protocol::ModelProvider::Api,
            profile: "fixture".into(),
            protocol: tickloop_agent_protocol::Protocol::Messages,
            requested_model: "fixture".into(),
        }
    }

    async fn complete(
        &self,
        request: ModelRequest,
        _: UnboundedSender<ModelDelta>,
        cancel: CancellationToken,
    ) -> Result<ModelOutput, ModelError> {
        self.requests.lock().unwrap().push(request);
        let step = self
            .steps
            .lock()
            .unwrap()
            .pop_front()
            .ok_or_else(|| ModelError::InvalidResponse("script exhausted".into()))?;
        match step {
            Step::Block => {
                self.entered.notify_one();
                cancel.cancelled().await;
                Err(ModelError::Cancelled)
            }
            Step::Reply(content) => Ok(ModelOutput {
                private_source: None,
                content,
                usage: Default::default(),
                stop_reason: "end_turn".into(),
            }),
        }
    }
}

fn message_text(messages: &[Message]) -> String {
    messages
        .iter()
        .flat_map(|message| &message.content)
        .filter_map(|content| match content {
            Content::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn pasted_image(id: u32) -> InputContext {
    let pixels = RgbImage::from_fn(4, 2, |x, y| Rgb([x as u8 * 60, y as u8 * 90, 30]));
    let mut bytes = Vec::new();
    DynamicImage::ImageRgb8(pixels)
        .write_to(&mut std::io::Cursor::new(&mut bytes), ImageFormat::Png)
        .unwrap();
    InputContext::Image(ImageContext {
        origin: ImageOrigin::Pasted { id },
        image: prepare(&bytes).unwrap(),
    })
}

async fn wait_for_run_end(events: &mut Receiver<Event>, run_id: Uuid) -> EventKind {
    loop {
        let event = next_event(events).await;
        if event.run_id == run_id
            && matches!(
                event.kind,
                EventKind::Completed { .. } | EventKind::Failed(_) | EventKind::Interrupted
            )
        {
            return event.kind;
        }
    }
}

async fn complete_prompt(
    agent: &Agent,
    events: &mut Receiver<Event>,
    chat: ChatId,
    goal: Option<Uuid>,
    prompt: &str,
) -> Receipt {
    let receipt = agent.submit(chat, goal, prompt.into()).await.unwrap();
    assert!(matches!(
        wait_for_run_end(events, receipt.run_id).await,
        EventKind::Completed { .. }
    ));
    receipt
}

/// Submits `prompt`, waits until its model request is in flight, and interrupts it.
async fn interrupt_before_reply(
    agent: &Agent,
    model: &StepModel,
    events: &mut Receiver<Event>,
    chat: ChatId,
    goal: Option<Uuid>,
    prompt: &str,
    contexts: Vec<InputContext>,
) -> Receipt {
    let receipt = agent
        .submit_with_input_contexts(chat, goal, prompt.into(), contexts, Vec::new())
        .await
        .unwrap();
    model.entered.notified().await;
    agent.interrupt(chat).await.unwrap();
    assert!(matches!(
        wait_for_run_end(events, receipt.run_id).await,
        EventKind::Interrupted
    ));
    receipt
}

#[tokio::test]
async fn a_follow_up_interrupted_before_any_reply_leaves_the_history_and_keeps_its_loop() {
    let directory = tempfile::tempdir().unwrap();
    let state = directory.path().join("state");
    let model = StepModel::new(vec![
        Step::Reply(text("kept answer")),
        Step::Block,
        Step::Reply(text("replacement answer")),
    ]);
    let agent = Agent::open_embedded(state.clone(), config(), model.clone()).unwrap();
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    let mut events = agent.subscribe();
    let kept = complete_prompt(&agent, &mut events, chat, None, "kept prompt").await;
    let before = agent.snapshot(chat).await.unwrap();
    let image = pasted_image(1);
    let interrupted = interrupt_before_reply(
        &agent,
        &model,
        &mut events,
        chat,
        Some(kept.loop_id),
        "taken back [Image #1]",
        vec![image.clone()],
    )
    .await;
    let interrupted_snapshot = agent.snapshot(chat).await.unwrap();
    assert!(message_text(&interrupted_snapshot.messages).contains(INTERRUPT_MESSAGE));

    let draft = agent
        .take_back_interrupted_prompt(chat, interrupted.run_id)
        .await
        .unwrap()
        .expect("a turn without a reply gives its prompt back");
    assert_eq!(draft.prompt, "taken back [Image #1]");
    assert_eq!(draft.contexts, vec![image]);
    assert_eq!(draft.loop_id, Some(kept.loop_id));
    let taken_back = agent.snapshot(chat).await.unwrap();
    assert_eq!(taken_back.messages, before.messages);
    assert_eq!(taken_back.rewind_points(), before.rewind_points());
    assert_eq!(
        taken_back
            .loops
            .iter()
            .map(|goal| goal.id)
            .collect::<Vec<_>>(),
        [kept.loop_id]
    );
    assert!(
        taken_back
            .events
            .iter()
            .all(|event| event.run_id != interrupted.run_id),
        "a replay no longer shows the taken-back turn"
    );
    assert!(
        taken_back
            .events
            .iter()
            .any(|event| matches!(event.kind, EventKind::ConversationRewound { .. })),
        "other Views redraw the conversation"
    );
    assert!(
        agent
            .take_back_interrupted_prompt(chat, interrupted.run_id)
            .await
            .unwrap()
            .is_none(),
        "the prompt is taken back once"
    );

    complete_prompt(
        &agent,
        &mut events,
        chat,
        draft.loop_id,
        "replacement prompt",
    )
    .await;
    let request = model.last_request_text();
    assert!(request.contains("kept answer"), "{request}");
    assert!(!request.contains("taken back"), "{request}");
    assert!(!request.contains(INTERRUPT_MESSAGE), "{request}");
    agent.shutdown().await;
    drop(agent);

    let reopened = Agent::open_embedded(state, config(), model.clone()).unwrap();
    let restored = reopened.resume_chat(chat).await.unwrap();
    assert_eq!(
        message_text(&restored.messages),
        "kept prompt\nkept answer\nreplacement prompt\nreplacement answer"
    );
    reopened.shutdown().await;
}

#[tokio::test]
async fn a_taken_back_first_prompt_leaves_neither_its_message_nor_its_loop() {
    let directory = tempfile::tempdir().unwrap();
    let model = StepModel::new(vec![Step::Block, Step::Reply(text("fresh answer"))]);
    let agent = Agent::new_embedded(config(), model.clone());
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    let mut events = agent.subscribe();
    let interrupted = interrupt_before_reply(
        &agent,
        &model,
        &mut events,
        chat,
        None,
        "first words",
        Vec::new(),
    )
    .await;
    let draft = agent
        .take_back_interrupted_prompt(chat, interrupted.run_id)
        .await
        .unwrap()
        .expect("the first prompt comes back");
    assert_eq!(draft.prompt, "first words");
    assert_eq!(
        draft.loop_id, None,
        "the prompt's own Loop is gone, so sending again starts a new one"
    );
    let taken_back = agent.snapshot(chat).await.unwrap();
    assert!(taken_back.messages.is_empty());
    assert!(taken_back.loops.is_empty());

    complete_prompt(&agent, &mut events, chat, None, "second words").await;
    let request = model.last_request_text();
    assert!(request.contains("second words"), "{request}");
    assert!(!request.contains("first words"), "{request}");
    agent.shutdown().await;
}

#[tokio::test]
async fn a_reply_a_queued_input_or_a_finished_turn_keeps_the_prompt() {
    let directory = tempfile::tempdir().unwrap();
    let model = StepModel::new(vec![
        Step::Reply(call(
            "write",
            "Write",
            serde_json::json!({"file_path": "needs-approval.txt", "content": "x"}),
        )),
        Step::Block,
        Step::Reply(text("queued answer")),
        Step::Reply(text("finished answer")),
    ]);
    let agent = Agent::new_embedded(config(), model.clone());
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

    let with_tool_call = agent
        .submit(chat, None, "prompt with a tool call".into())
        .await
        .unwrap();
    loop {
        let event = next_event(&mut events).await;
        if event.run_id == with_tool_call.run_id
            && matches!(event.kind, EventKind::PermissionRequested { .. })
        {
            break;
        }
    }
    agent.interrupt(chat).await.unwrap();
    assert!(matches!(
        wait_for_run_end(&mut events, with_tool_call.run_id).await,
        EventKind::Interrupted
    ));
    assert!(
        agent
            .take_back_interrupted_prompt(chat, with_tool_call.run_id)
            .await
            .unwrap()
            .is_none(),
        "a tool call is a reply"
    );

    let blocked = agent
        .submit(chat, None, "prompt before a queued input".into())
        .await
        .unwrap();
    model.entered.notified().await;
    let queued = agent
        .submit(chat, Some(blocked.loop_id), "queued input".into())
        .await
        .unwrap();
    agent.interrupt(chat).await.unwrap();
    assert!(matches!(
        wait_for_run_end(&mut events, blocked.run_id).await,
        EventKind::Interrupted
    ));
    assert!(
        agent
            .take_back_interrupted_prompt(chat, blocked.run_id)
            .await
            .unwrap()
            .is_none(),
        "the queued input already moved on"
    );
    assert!(matches!(
        wait_for_run_end(&mut events, queued.run_id).await,
        EventKind::Completed { .. }
    ));
    assert!(
        agent
            .take_back_interrupted_prompt(chat, blocked.run_id)
            .await
            .unwrap()
            .is_none(),
        "a later prompt follows the interrupted one"
    );

    let finished = complete_prompt(&agent, &mut events, chat, None, "finished prompt").await;
    assert!(
        agent
            .take_back_interrupted_prompt(chat, finished.run_id)
            .await
            .unwrap()
            .is_none(),
        "only an interrupted turn gives its prompt back"
    );
    let conversation = message_text(&agent.snapshot(chat).await.unwrap().messages);
    for prompt in [
        "prompt with a tool call",
        "prompt before a queued input",
        "queued input",
        "finished prompt",
    ] {
        assert!(conversation.contains(prompt), "{prompt}: {conversation}");
    }
    agent.shutdown().await;
}

#[tokio::test]
async fn guidance_written_after_the_prompt_is_no_reply_and_leaves_with_it() {
    let directory = tempfile::tempdir().unwrap();
    let model = StepModel::new(vec![Step::Block]);
    let agent =
        Agent::open_embedded(directory.path().join("state"), config(), model.clone()).unwrap();
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    let mut events = agent.subscribe();
    let receipt = agent
        .invoke_skill(chat, None, "/tickloop", "show the current Workspace")
        .await
        .unwrap();
    model.entered.notified().await;
    assert!(
        model
            .last_request_text()
            .contains("Reference guidance from tickloop")
    );
    agent.interrupt(chat).await.unwrap();
    assert!(matches!(
        wait_for_run_end(&mut events, receipt.run_id).await,
        EventKind::Interrupted
    ));
    let draft = agent
        .take_back_interrupted_prompt(chat, receipt.run_id)
        .await
        .unwrap()
        .expect("a guidance note after the prompt is not a reply");
    assert_eq!(draft.prompt, "/tickloop show the current Workspace");
    let taken_back = agent.snapshot(chat).await.unwrap();
    assert!(taken_back.messages.is_empty(), "{:?}", taken_back.messages);
    assert!(taken_back.host_notes.is_empty());
}
