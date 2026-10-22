//! Test code shared by the Agent crates' unit and integration tests: a scripted model with its
//! replies, runtime events, Transcript files and stores, a reference stateless host with its
//! Event Stream, PTY screens, a terminal's kitty keyboard flags, transport limits and model HTTP
//! servers. Helpers that only one crate's tests use stay in that crate.

pub mod event_stream;
pub mod harness;
pub mod input_dialog;
pub mod kitty_keyboard;
pub mod legacy_models;
pub mod model_http_support;
pub mod routed;
pub mod screen;
pub mod stateless_host;
pub mod transcripts;

/// A short canonical directory for test sockets, whose paths are limited to about 100 bytes;
/// macOS's `/tmp` is a symbolic link to `/private/tmp`.
#[cfg(target_os = "macos")]
pub const SHORT_TEMP_ROOT: &str = "/private/tmp";
#[cfg(not(target_os = "macos"))]
pub const SHORT_TEMP_ROOT: &str = "/tmp";

use async_trait::async_trait;
use std::{collections::VecDeque, sync::Mutex};
use tickloop_agent_model::*;
use tickloop_agent_protocol::{Content, InputModality, ModelConfig, Protocol, Role, Usage};
use tokio::sync::mpsc::UnboundedSender;
use tokio_util::sync::CancellationToken;

/// Small transport limits that let tests cross the frame and chunk boundaries with little data.
pub const TRANSPORT_LIMITS: tickloop_agent_protocol::AccessLimits =
    match tickloop_agent_protocol::AccessLimits::new(64 << 10, 16 << 10) {
        Ok(limits) => limits,
        Err(_) => panic!("the test AccessLimits are valid"),
    };

/// The libtest name of a test function of the calling test binary, for tests that re-execute
/// themselves with `--exact`; naming a function that does not exist fails to compile.
#[macro_export]
macro_rules! test_path {
    ($test:ident) => {{
        let _: fn() = $test;
        format!(
            "{}::{}",
            module_path!()
                .strip_prefix(concat!(env!("CARGO_CRATE_NAME"), "::"))
                .unwrap(),
            stringify!($test)
        )
    }};
}

pub struct ScriptedModel {
    pub requests: Mutex<Vec<ModelRequest>>,
    /// Title requests, which never consume a scripted reply.
    pub title_requests: Mutex<Vec<ModelRequest>>,
    replies: Mutex<VecDeque<Vec<Content>>>,
    usages: Mutex<VecDeque<Usage>>,
    rejections: Mutex<VecDeque<ModelResponseRejection>>,
}

impl ScriptedModel {
    pub fn new(replies: Vec<Vec<Content>>) -> Self {
        Self::with_usages(replies, Vec::new())
    }

    pub fn with_usages(replies: Vec<Vec<Content>>, usages: Vec<Usage>) -> Self {
        Self {
            requests: Mutex::new(Vec::new()),
            title_requests: Mutex::new(Vec::new()),
            replies: Mutex::new(replies.into()),
            usages: Mutex::new(usages.into()),
            rejections: Mutex::new(VecDeque::new()),
        }
    }

    pub fn enqueue(&self, reply: Vec<Content>) {
        self.replies.lock().unwrap().push_back(reply);
    }

    /// Rejects the next request, which is still recorded, without consuming a scripted reply. The
    /// error carries the response-header stage, as the HTTP clients report a rejected response.
    pub fn reject_next(&self, rejection: ModelResponseRejection) {
        self.rejections.lock().unwrap().push_back(rejection);
    }
}

#[async_trait]
impl ModelClient for ScriptedModel {
    async fn generate_title(
        &self,
        request: tickloop_agent_model::ModelRequest,
        _: tokio::sync::mpsc::UnboundedSender<tickloop_agent_model::ModelDelta>,
        _: tokio_util::sync::CancellationToken,
    ) -> Result<tickloop_agent_model::ModelOutput, tickloop_agent_model::ModelError> {
        let output = title_output(request.clone());
        self.title_requests.lock().unwrap().push(request);
        Ok(output)
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
        events: UnboundedSender<ModelDelta>,
        _: CancellationToken,
    ) -> Result<ModelOutput, ModelError> {
        let plan_file = reminded_plan_file(&request);
        self.requests.lock().unwrap().push(request);
        if let Some(rejection) = self.rejections.lock().unwrap().pop_front() {
            return Err(ModelError::ResponseRejected(rejection)
                .at_stage(ModelRequestStage::ResponseHeaders));
        }
        let mut content = self
            .replies
            .lock()
            .unwrap()
            .pop_front()
            .ok_or_else(|| ModelError::InvalidResponse("script exhausted".into()))?;
        if let Some(plan_file) = plan_file {
            for block in &mut content {
                if let Content::ToolUse { input, .. } = block
                    && input["file_path"] == PLAN_FILE
                {
                    input["file_path"] = plan_file.clone().into();
                }
            }
        }
        for block in &content {
            if let Content::Text { text } = block {
                let _ = events.send(ModelDelta::Text(text.clone()));
            }
        }
        Ok(ModelOutput {
            private_source: None,
            content,
            usage: self.usages.lock().unwrap().pop_front().unwrap_or_default(),
            stop_reason: "end_turn".into(),
        })
    }
}

pub fn config() -> ModelConfig {
    ModelConfig {
        provider: tickloop_agent_protocol::ModelProvider::Api,
        auth_source: tickloop_agent_protocol::ModelAuthSource::ApiKeyEnv,
        name: "fixture".into(),
        protocol: Protocol::Messages,
        base_url: "http://127.0.0.1".into(),
        model: "fixture".into(),
        api_key_env: String::new(),
        max_output_tokens: 1024,
        provider_id: String::new(),
        provider_name: String::new(),
        model_id: String::new(),
        connection: None,
        card: None,
        input_modalities: vec![InputModality::Text, InputModality::Image],
    }
}
/// A transport fixture using a valid built-in selection identity; its parameters stay in memory.
pub fn persisted_config() -> ModelConfig {
    let mut model = config();
    model.provider_id = "deepseek-api".into();
    model.provider_name = "DeepSeek API".into();
    model.model_id = "deepseek-flash".into();
    model.model = "deepseek-flash".into();
    model
}

/// Declares the test program's context budget instead of saving a retired user override.
pub fn config_with_window(tokens: usize) -> ModelConfig {
    let mut model = tickloop_agent_model::cards::model_for_persistent_selection(persisted_config());
    model.card.as_mut().unwrap().context_window_tokens = Some(tokens);
    model
}

pub fn text(value: &str) -> Vec<Content> {
    vec![Content::Text { text: value.into() }]
}

pub fn auto_classification(block: bool) -> serde_json::Value {
    if block {
        serde_json::json!({"decision":"block_soft", "reason":"The operation matches the external-code restriction without authorization", "rule_id":"builtin.soft_deny.code_from_external"})
    } else {
        serde_json::json!({"decision":"allow", "reason":null, "rule_id":null})
    }
}
/// `AskUserQuestion` input with one single-select question offering "Weekly" and "Monthly".
pub fn single_question(question: &str) -> serde_json::Value {
    serde_json::json!({"questions":[{"question":question,"header":"Report","options":[
        {"label":"Weekly","description":"The Monday summary"},
        {"label":"Monthly","description":"The board pack"}
    ]}]})
}

pub fn call(id: &str, name: &str, input: serde_json::Value) -> Vec<Content> {
    vec![Content::ToolUse {
        id: id.into(),
        name: name.into(),
        input,
    }]
}

/// Stands for the current Loop's plan file in a scripted `file_path`. `ScriptedModel` replaces it
/// with the path named by the plan-mode reminder of the request it answers, as a model would.
pub const PLAN_FILE: &str = "<plan file>";

pub fn write_plan(id: &str, markdown: &str) -> Vec<Content> {
    call(
        id,
        "Write",
        serde_json::json!({"file_path": PLAN_FILE, "content": markdown}),
    )
}

pub fn exit_plan_mode(id: &str) -> Vec<Content> {
    call(id, "ExitPlanMode", serde_json::json!({}))
}

/// One reply that writes the plan file (call `{id}-write`) and submits it with `ExitPlanMode`
/// (call `id`).
pub fn submit_plan(id: &str, markdown: &str) -> Vec<Content> {
    [
        write_plan(&format!("{id}-write"), markdown),
        exit_plan_mode(id),
    ]
    .concat()
}

/// The plan file path named by the plan-mode reminder in a request's system prompt.
pub fn reminded_plan_file(request: &ModelRequest) -> Option<String> {
    let system = request
        .messages
        .iter()
        .filter(|message| message.role == Role::System)
        .flat_map(|message| &message.content)
        .find_map(|content| match content {
            Content::Text { text } => Some(text.as_str()),
            _ => None,
        })?;
    [
        ("create your plan at ", " using the Write tool."),
        ("A plan file already exists at ", ". You can read it"),
    ]
    .into_iter()
    .find_map(|(before, after)| {
        let (_, rest) = system.split_once(before)?;
        Some(rest.split_once(after)?.0.to_owned())
    })
}

/// How long [`next_event`] waits for the next runtime event.
const EVENT_WAIT: std::time::Duration = std::time::Duration::from_secs(5);

pub async fn next_event(
    receiver: &mut tokio::sync::broadcast::Receiver<tickloop_agent_protocol::Event>,
) -> tickloop_agent_protocol::Event {
    next_event_within(receiver, EVENT_WAIT).await
}

pub async fn next_event_within(
    receiver: &mut tokio::sync::broadcast::Receiver<tickloop_agent_protocol::Event>,
    wait: std::time::Duration,
) -> tickloop_agent_protocol::Event {
    tokio::time::timeout(wait, receiver.recv())
        .await
        .expect("event timed out")
        .expect("event channel closed")
}

/// Transcript 夹具：按 id 在 `<root>/projects/*/` 中查找 Chat 的 Transcript 文件。
pub fn transcript_file(root: &std::path::Path, chat: uuid::Uuid) -> std::path::PathBuf {
    std::fs::read_dir(root.join("projects"))
        .unwrap()
        .map(|project| project.unwrap().path().join(format!("{chat}.jsonl")))
        .find(|path| path.is_file())
        .unwrap_or_else(|| panic!("Transcript for Chat {chat} does not exist"))
}

/// 从第一条满足条件的行开始截断 Transcript，模拟进程在这些条目写入前崩溃。
pub fn truncate_transcript_before(path: &std::path::Path, is_cut: impl Fn(&str) -> bool) {
    let text = std::fs::read_to_string(path).unwrap();
    let kept = text
        .lines()
        .take_while(|line| !is_cut(line))
        .map(|line| format!("{line}\n"))
        .collect::<String>();
    assert!(
        kept.len() < text.len(),
        "no Transcript line matched the cut"
    );
    std::fs::write(path, kept).unwrap();
}

/// 匹配某个工具调用的结果条目。
pub fn is_tool_result_line(line: &str, call_id: &str) -> bool {
    line.contains("\"tool_result\"") && line.contains(&format!("\"tool_use_id\":\"{call_id}\""))
}

pub fn title_output(request: ModelRequest) -> ModelOutput {
    let content = request
        .messages
        .into_iter()
        .rev()
        .find(|message| message.role == Role::User)
        .map(|message| message.content)
        .unwrap_or_default();
    ModelOutput {
        private_source: None,
        content,
        usage: Usage {
            input_tokens: Some(0),
            output_tokens: Some(0),
            cache_read_input_tokens: Some(0),
            cache_write_input_tokens: Some(0),
        },
        stop_reason: "end_turn".into(),
    }
}
