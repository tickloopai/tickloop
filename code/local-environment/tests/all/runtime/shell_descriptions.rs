use std::{sync::Arc, sync::Mutex, time::Duration};
use tickloop_code_local_environment::AgentLocalExt;

use async_trait::async_trait;
use serde_json::{Value, json};
use tickloop_harness::model::{ModelClient, ModelDelta, ModelError, ModelOutput, ModelRequest};
use tickloop_harness::protocol::{
    ChatId, Content, EventKind, InputModality, ModelConfig, Protocol, Role, ShellTask,
    ShellTaskState, ShellTaskSubscriptionEvent, Usage, UserShellState,
};
use tickloop_harness::runtime::{Agent, ShellTaskCenter};
use tokio::sync::mpsc::UnboundedSender;
use tokio_util::sync::CancellationToken;

const WAIT: Duration = Duration::from_secs(5);
const SETTLE: Duration = Duration::from_millis(300);

/// Runs one Bash call when given, and answers Shell description requests with `description`
/// (or a non-retryable failure when it is `None`).
struct DescribingModel {
    bash: Option<Value>,
    description: Option<&'static str>,
    description_requests: Mutex<Vec<ModelRequest>>,
}

impl DescribingModel {
    fn new(bash: Option<Value>, description: Option<&'static str>) -> Arc<Self> {
        Arc::new(Self {
            bash,
            description,
            description_requests: Mutex::new(Vec::new()),
        })
    }

    fn description_requests(&self) -> usize {
        self.description_requests.lock().unwrap().len()
    }
}

fn is_description_request(request: &ModelRequest) -> bool {
    request.messages.iter().any(|message| {
        message.role == Role::System
            && message.content.iter().any(|content| {
                matches!(content, Content::Text { text } if text.contains("shell command"))
            })
    })
}

fn text_output(text: &str) -> ModelOutput {
    ModelOutput {
        private_source: None,
        content: vec![Content::Text { text: text.into() }],
        usage: Usage::default(),
        stop_reason: "end_turn".into(),
    }
}

#[async_trait]
impl ModelClient for DescribingModel {
    async fn generate_title(
        &self,
        request: ModelRequest,
        _: UnboundedSender<ModelDelta>,
        _: CancellationToken,
    ) -> Result<ModelOutput, ModelError> {
        if !is_description_request(&request) {
            return Ok(tickloop_harness::model::fixture_title(request));
        }
        self.description_requests.lock().unwrap().push(request);
        self.description
            .map(text_output)
            .ok_or_else(|| ModelError::Configuration("no description fixture".into()))
    }

    fn identity(&self) -> tickloop_harness::protocol::ModelIdentity {
        tickloop_harness::protocol::ModelIdentity {
            provider: tickloop_harness::protocol::ModelProvider::Api,
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
        let has_result = request
            .messages
            .iter()
            .flat_map(|message| &message.content)
            .any(|content| matches!(content, Content::ToolResult { .. }));
        Ok(match (&self.bash, has_result) {
            (Some(input), false) => ModelOutput {
                private_source: None,
                content: vec![Content::ToolUse {
                    id: "shell-call".into(),
                    name: "Bash".into(),
                    input: input.clone(),
                }],
                usage: Usage::default(),
                stop_reason: "end_turn".into(),
            },
            _ => text_output("done"),
        })
    }
}

fn config() -> ModelConfig {
    ModelConfig {
        provider: tickloop_harness::protocol::ModelProvider::Api,
        auth_source: tickloop_harness::protocol::ModelAuthSource::ApiKeyEnv,
        name: "test".into(),
        protocol: Protocol::Messages,
        base_url: "http://127.0.0.1".into(),
        model: "test".into(),
        api_key_env: String::new(),
        max_output_tokens: 1024,
        provider_id: String::new(),
        provider_name: String::new(),
        model_id: String::new(),
        connection: None,
        card: None,
        input_modalities: vec![InputModality::Text],
    }
}

/// Submits a prompt and approves the Bash call it leads to.
async fn run_bash_call(agent: &Agent, chat: ChatId) {
    let mut events = agent.subscribe();
    agent.submit(chat, "Run it".into()).await.unwrap();
    tokio::time::timeout(WAIT, async {
        loop {
            match events.recv().await.unwrap().kind {
                EventKind::PermissionRequested { request } => {
                    agent
                        .answer_permission(chat, request.id, true)
                        .await
                        .unwrap();
                }
                EventKind::CommandUpdated { .. } => return,
                _ => {}
            }
        }
    })
    .await
    .expect("the Bash call never started a command");
}

/// Waits until the Chat's Shell task matching `ready` appears in the Task Center.
async fn wait_for_task(
    center: &ShellTaskCenter,
    chat: ChatId,
    ready: impl Fn(&ShellTask) -> bool,
) -> ShellTask {
    tokio::time::timeout(WAIT, async {
        loop {
            if let Some(task) = center
                .snapshot(chat)
                .await
                .unwrap()
                .tasks
                .into_iter()
                .find(|task| ready(task))
            {
                return task;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the expected Shell task never appeared")
}

#[tokio::test]
async fn a_bash_description_names_the_task_from_creation_through_backgrounding() {
    let directory = tempfile::tempdir().unwrap();
    let model = DescribingModel::new(
        Some(json!({"command": "printf ready; sleep 30", "description": "Wait for the fixture"})),
        Some("Unused"),
    );
    let agent = Agent::new_embedded(config(), model.clone());
    agent.install_own_tickhost();
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    let center = ShellTaskCenter::new(agent.clone());
    run_bash_call(&agent, chat).await;
    let running = wait_for_task(&center, chat, |task| {
        task.state == ShellTaskState::Running && task.output_bytes >= 5
    })
    .await;
    assert_eq!(running.description.as_deref(), Some("Wait for the fixture"));

    agent.background_command(chat, running.id).await.unwrap();
    let background = wait_for_task(&center, chat, |task| {
        task.id == running.id && task.state == ShellTaskState::Background
    })
    .await;
    assert_eq!(
        background.description.as_deref(),
        Some("Wait for the fixture")
    );
    tokio::time::sleep(SETTLE).await;
    assert_eq!(model.description_requests(), 0);
    agent.stop_command(chat, running.id).await.unwrap();
    agent.shutdown().await;
}

#[tokio::test]
async fn an_undescribed_background_bash_call_is_named_by_the_model() {
    let directory = tempfile::tempdir().unwrap();
    let model = DescribingModel::new(
        Some(json!({"command": "sleep 30", "run_in_background": true})),
        Some("\"Sleep for thirty seconds.\""),
    );
    let agent = Agent::new_embedded(config(), model.clone());
    agent.install_own_tickhost();
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    let center = ShellTaskCenter::new(agent.clone());
    run_bash_call(&agent, chat).await;
    let named = wait_for_task(&center, chat, |task| task.description.is_some()).await;
    assert_eq!(
        named.description.as_deref(),
        Some("Sleep for thirty seconds")
    );
    assert_eq!(named.command, "sleep 30");
    assert_eq!(model.description_requests(), 1);
    agent.stop_command(chat, named.id).await.unwrap();
    // The model-facing notification still names the command itself.
    tokio::time::timeout(WAIT, async {
        loop {
            let texts = agent
                .snapshot(chat)
                .await
                .unwrap()
                .messages
                .into_iter()
                .flat_map(|message| message.content)
                .filter_map(|content| match content {
                    Content::Text { text } => Some(text),
                    _ => None,
                })
                .collect::<Vec<_>>();
            if texts
                .iter()
                .any(|text| text.contains("<summary>Background command \"sleep 30\""))
            {
                return;
            }
            assert!(
                !texts.iter().any(|text| text.contains("Sleep for thirty")),
                "{texts:?}"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the completion notification never named the command");
    agent.shutdown().await;
}

#[tokio::test]
async fn a_user_shell_moved_to_background_gets_one_generated_description() {
    let directory = tempfile::tempdir().unwrap();
    let model = DescribingModel::new(None, Some("Print x in a loop"));
    let agent = Agent::new_embedded(config(), model.clone());
    agent.install_own_tickhost();
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    let center = ShellTaskCenter::new(agent.clone());
    let command = "while :; do printf x; sleep 0.02; done";
    let receipt = agent
        .submit_user_shell(chat, command.into(), false)
        .await
        .unwrap();
    let running = wait_for_task(&center, chat, |task| {
        task.id == receipt.command_id
            && task.state == ShellTaskState::Running
            && task.output_bytes > 0
    })
    .await;
    assert_eq!(running.description, None);
    assert_eq!(
        model.description_requests(),
        0,
        "foreground Shells stay unnamed"
    );

    let mut subscription = center.subscribe(chat).await.unwrap();
    let background = agent
        .background_user_shell(chat, receipt.command_id)
        .await
        .unwrap();
    assert_eq!(background.state, UserShellState::Background);
    let named = tokio::time::timeout(WAIT, async {
        loop {
            if let ShellTaskSubscriptionEvent::Changed(delta) = subscription.recv().await.unwrap()
                && delta.task.id == receipt.command_id
                && delta.task.description.is_some()
            {
                return delta.task;
            }
        }
    })
    .await
    .expect("subscribers never saw the generated description");
    assert_eq!(named.description.as_deref(), Some("Print x in a loop"));
    let requests = model.description_requests.lock().unwrap().clone();
    assert_eq!(requests.len(), 1);
    assert!(requests[0].messages.iter().any(|message| {
        message.role == Role::User
            && message
                .content
                .iter()
                .any(|content| matches!(content, Content::Text { text } if text == command))
    }));

    // Later output updates neither ask again nor drop the name.
    let grown = named.output_bytes;
    let later = wait_for_task(&center, chat, |task| {
        task.id == receipt.command_id && task.output_bytes > grown
    })
    .await;
    assert_eq!(later.description.as_deref(), Some("Print x in a loop"));
    assert_eq!(model.description_requests(), 1);
    agent
        .stop_user_shell(chat, receipt.command_id)
        .await
        .unwrap();
    agent.shutdown().await;
}

#[tokio::test]
async fn a_failed_description_request_leaves_the_task_unnamed_without_retrying() {
    let directory = tempfile::tempdir().unwrap();
    let model = DescribingModel::new(None, None);
    let agent = Agent::new_embedded(config(), model.clone());
    agent.install_own_tickhost();
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    let center = ShellTaskCenter::new(agent.clone());
    let receipt = agent
        .submit_user_shell(chat, "while :; do printf x; sleep 0.02; done".into(), true)
        .await
        .unwrap();
    let started = wait_for_task(&center, chat, |task| {
        task.id == receipt.command_id
            && task.state == ShellTaskState::Background
            && task.output_bytes > 0
    })
    .await;
    tokio::time::timeout(WAIT, async {
        while model.description_requests() == 0 {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("no description was requested");
    wait_for_task(&center, chat, |task| {
        task.id == receipt.command_id && task.output_bytes > started.output_bytes
    })
    .await;
    tokio::time::sleep(SETTLE).await;
    assert_eq!(model.description_requests(), 1);
    let task = wait_for_task(&center, chat, |task| task.id == receipt.command_id).await;
    assert_eq!(task.description, None);
    agent
        .stop_user_shell(chat, receipt.command_id)
        .await
        .unwrap();
    agent.shutdown().await;
}
