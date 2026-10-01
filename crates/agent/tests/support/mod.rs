#![allow(dead_code)]

use async_trait::async_trait;
use std::{collections::VecDeque, sync::Mutex};
use tickloop_agent::model::*;
use tokio::sync::mpsc::UnboundedSender;
use tokio_util::sync::CancellationToken;

pub struct ScriptedModel {
    pub requests: Mutex<Vec<ModelRequest>>,
    replies: Mutex<VecDeque<Vec<Content>>>,
    usages: Mutex<VecDeque<Usage>>,
}

impl ScriptedModel {
    pub fn new(replies: Vec<Vec<Content>>) -> Self {
        Self::with_usages(replies, Vec::new())
    }

    pub fn with_usages(replies: Vec<Vec<Content>>, usages: Vec<Usage>) -> Self {
        Self {
            requests: Mutex::new(Vec::new()),
            replies: Mutex::new(replies.into()),
            usages: Mutex::new(usages.into()),
        }
    }
}

#[async_trait]
impl ModelClient for ScriptedModel {
    fn identity(&self) -> tickloop_agent::model::ModelIdentity {
        tickloop_agent::model::ModelIdentity {
            provider: tickloop_agent::model::ModelProvider::Api,
            profile: "fixture".into(),
            protocol: tickloop_agent::model::Protocol::Messages,
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
        provider: tickloop_agent::model::ModelProvider::Api,
        auth_source: tickloop_agent::model::ModelAuthSource::ApiKeyEnv,
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
pub fn text(value: &str) -> Vec<Content> {
    vec![Content::Text { text: value.into() }]
}

pub fn auto_findings(block: bool) -> serde_json::Value {
    let rules = [
        "external_code",
        "destruction",
        "persistence",
        "security",
        "exfiltration",
        "credentials",
        "external_scope",
        "finance",
    ]
    .into_iter()
    .enumerate()
    .map(|(index, id)| {
        serde_json::json!({
            "id": format!("builtin.soft_deny.{id}"),
            "applicability": if block && index == 0 { "present" } else { "absent" },
            "clearance": "missing"
        })
    })
    .collect::<Vec<_>>();
    serde_json::json!({"task_scope":"within_task", "rules":rules})
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

pub async fn next_event(
    receiver: &mut tokio::sync::broadcast::Receiver<tickloop_agent::runtime::Event>,
) -> tickloop_agent::runtime::Event {
    tokio::time::timeout(std::time::Duration::from_secs(5), receiver.recv())
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
