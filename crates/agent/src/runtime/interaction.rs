use super::*;
use crate::{model::ToolDefinition, tools::ToolResult};
use serde_json::{Value, json};
use std::collections::HashSet;

pub const ASK_USER_QUESTION_TOOL_NAME: &str = "AskUserQuestion";
pub const START_GOAL_TOOL_NAME: &str = "StartGoal";
const QUESTION_HEADER_MAX_CHARACTERS: usize = 12;
const MIN_QUESTIONS: usize = 1;
const MAX_QUESTIONS: usize = 4;
const MIN_QUESTION_OPTIONS: usize = 2;
const MAX_QUESTION_OPTIONS: usize = 4;
const MAX_ANSWER_BYTES: usize = 65536;
const UNIQUENESS_ERROR: &str =
    "Question texts must be unique, option labels must be unique within each question";

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct QuestionRequest {
    pub id: Uuid,
    pub chat_id: ChatId,
    pub loop_id: LoopId,
    pub run_id: RunId,
    pub call_id: String,
    pub questions: Vec<Question>,
}

/// One question of an `AskUserQuestion` call, shaped like Claude Code's `questionSchema`.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct Question {
    pub question: String,
    pub header: String,
    pub options: Vec<QuestionOption>,
    #[serde(rename = "multiSelect", default)]
    pub multi_select: bool,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct QuestionOption {
    pub label: String,
    pub description: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preview: Option<String>,
}

/// The user's answer to one question: chosen option labels, in the order they were chosen,
/// and optional free text entered through the automatic "Other" choice.
#[derive(Clone, Debug, Default, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct QuestionAnswer {
    pub options: Vec<String>,
    pub other: Option<String>,
}

impl QuestionAnswer {
    pub fn option(label: impl Into<String>) -> Self {
        Self {
            options: vec![label.into()],
            other: None,
        }
    }

    pub fn options<Label: Into<String>>(labels: impl IntoIterator<Item = Label>) -> Self {
        Self {
            options: labels.into_iter().map(Into::into).collect(),
            other: None,
        }
    }

    pub fn other(text: impl Into<String>) -> Self {
        Self {
            options: Vec::new(),
            other: Some(text.into()),
        }
    }

    pub fn with_other(mut self, text: impl Into<String>) -> Self {
        self.other = Some(text.into());
        self
    }

    /// Claude Code joins a multi-select answer, including the "Other" text, with ", ".
    pub fn text(&self) -> String {
        self.options
            .iter()
            .map(String::as_str)
            .chain(self.other.as_deref())
            .collect::<Vec<_>>()
            .join(", ")
    }

    fn validate(&self, question: &Question) -> Result<(), String> {
        let mut chosen = HashSet::new();
        for label in &self.options {
            if !question.options.iter().any(|option| option.label == *label) {
                return Err(format!(
                    "\"{label}\" is not an option of \"{}\"",
                    question.question
                ));
            }
            if !chosen.insert(label) {
                return Err(format!("\"{label}\" is chosen more than once"));
            }
        }
        if self
            .other
            .as_ref()
            .is_some_and(|text| text.trim().is_empty())
        {
            return Err("an \"Other\" answer needs text".into());
        }
        let choice_count = self.options.len() + usize::from(self.other.is_some());
        if choice_count == 0 {
            return Err(format!("\"{}\" has no answer", question.question));
        }
        if !question.multi_select && choice_count > 1 {
            return Err(format!(
                "\"{}\" accepts one option or one \"Other\" answer",
                question.question
            ));
        }
        Ok(())
    }
}

/// `answers`, `annotations` and `metadata` are accepted by the input schema for Claude Code
/// compatibility; its permission component fills them, so the host ignores them here.
#[derive(Deserialize)]
struct AskUserQuestionInput {
    questions: Vec<Question>,
}

fn has_unique_questions_and_labels(questions: &[Question]) -> bool {
    let mut texts = HashSet::new();
    questions.iter().all(|question| {
        let mut labels = HashSet::new();
        texts.insert(&question.question)
            && question
                .options
                .iter()
                .all(|option| labels.insert(&option.label))
    })
}

/// Mirrors `mapToolResultToToolResultBlockParam`: an answer that names an option with a
/// preview carries that preview, the way Claude Code reports a previewed selection.
fn answered_questions_result(questions: &[Question], answers: &[QuestionAnswer]) -> String {
    let answers_text = questions
        .iter()
        .zip(answers)
        .map(|(question, answer)| {
            let answer = answer.text();
            let preview = question
                .options
                .iter()
                .find(|option| option.label == answer)
                .and_then(|option| option.preview.as_deref());
            match preview {
                Some(preview) => format!(
                    "\"{}\"=\"{answer}\" selected preview:\n{preview}",
                    question.question
                ),
                None => format!("\"{}\"=\"{answer}\"", question.question),
            }
        })
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "User has answered your questions: {answers_text}. You can now continue with the user's answers in mind."
    )
}

fn ask_user_question_definition() -> ToolDefinition {
    let plan_tool = super::plan::EXIT_PLAN_MODE_TOOL_NAME;
    ToolDefinition {
        name: ASK_USER_QUESTION_TOOL_NAME.into(),
        description: format!(
            "Use this tool when you need to ask the user questions during execution. This allows you to:
1. Gather user preferences or requirements
2. Clarify ambiguous instructions
3. Get decisions on implementation choices as you work
4. Offer choices to the user about what direction to take.

Usage notes:
- Users will always be able to select \"Other\" to provide custom text input
- Use multiSelect: true to allow multiple answers to be selected for a question
- If you recommend a specific option, make that the first option in the list and add \"(Recommended)\" at the end of the label

Plan mode note: In plan mode, use this tool to clarify requirements or choose between approaches BEFORE finalizing your plan. Do NOT use this tool to ask \"Is my plan ready?\" or \"Should I proceed?\" - use {plan_tool} for plan approval. IMPORTANT: Do not reference \"the plan\" in your questions (e.g., \"Do you have feedback about the plan?\", \"Does the plan look good?\") because the user cannot see the plan in the UI until you call {plan_tool}. If you need plan approval, use {plan_tool} instead.
"
        ),
        input_schema: json!({
            "type": "object",
            "properties": {
                "questions": {
                    "type": "array",
                    "minItems": MIN_QUESTIONS,
                    "maxItems": MAX_QUESTIONS,
                    "description": "Questions to ask the user (1-4 questions)",
                    "items": {
                        "type": "object",
                        "properties": {
                            "question": {
                                "type": "string",
                                "description": "The complete question to ask the user. Should be clear, specific, and end with a question mark. Example: \"Which library should we use for date formatting?\" If multiSelect is true, phrase it accordingly, e.g. \"Which features do you want to enable?\""
                            },
                            "header": {
                                "type": "string",
                                "description": format!("Very short label displayed as a chip/tag (max {QUESTION_HEADER_MAX_CHARACTERS} chars). Examples: \"Auth method\", \"Library\", \"Approach\".")
                            },
                            "options": {
                                "type": "array",
                                "minItems": MIN_QUESTION_OPTIONS,
                                "maxItems": MAX_QUESTION_OPTIONS,
                                "description": "The available choices for this question. Must have 2-4 options. Each option should be a distinct, mutually exclusive choice (unless multiSelect is enabled). There should be no 'Other' option, that will be provided automatically.",
                                "items": {
                                    "type": "object",
                                    "properties": {
                                        "label": {
                                            "type": "string",
                                            "description": "The display text for this option that the user will see and select. Should be concise (1-5 words) and clearly describe the choice."
                                        },
                                        "description": {
                                            "type": "string",
                                            "description": "Explanation of what this option means or what will happen if chosen. Useful for providing context about trade-offs or implications."
                                        },
                                        "preview": {
                                            "type": "string",
                                            "description": "Optional preview content shown as plain text below this option. Use for mockups, code snippets, or visual comparisons that help users compare options."
                                        }
                                    },
                                    "required": ["label", "description"]
                                }
                            },
                            "multiSelect": {
                                "type": "boolean",
                                "default": false,
                                "description": "Set to true to allow the user to select multiple options instead of just one. Use when choices are not mutually exclusive."
                            }
                        },
                        "required": ["question", "header", "options"]
                    }
                },
                "answers": {
                    "type": "object",
                    "additionalProperties": {"type": "string"},
                    "description": "User answers collected by the permission component"
                },
                "annotations": {
                    "type": "object",
                    "additionalProperties": {
                        "type": "object",
                        "properties": {
                            "preview": {
                                "type": "string",
                                "description": "The preview content of the selected option, if the question used previews."
                            },
                            "notes": {
                                "type": "string",
                                "description": "Free-text notes the user added to their selection."
                            }
                        }
                    },
                    "description": "Optional per-question annotations from the user (e.g., notes on preview selections). Keyed by question text."
                },
                "metadata": {
                    "type": "object",
                    "properties": {
                        "source": {
                            "type": "string",
                            "description": "Optional identifier for the source of this question (e.g., \"remember\" for /remember command). Used for analytics tracking."
                        }
                    },
                    "description": "Optional metadata for tracking and analytics purposes. Not displayed to user."
                }
            },
            "required": ["questions"],
            "additionalProperties": false
        }),
    }
}

pub(super) fn definitions() -> Vec<ToolDefinition> {
    vec![ask_user_question_definition(), ToolDefinition {
        name: START_GOAL_TOOL_NAME.into(),
        description: "Create and queue a separate, explicitly requested independent user goal in this Chat. Follow-up requirements for the current goal stay on the current goal. Ask the user when attribution is unclear; do not execute this new goal's work in the current run.".into(),
        input_schema: json!({"type":"object","properties":{"goal":{"type":"string"}},"required":["goal"],"additionalProperties":false}),
    }]
}

impl Agent {
    /// Submits the user's answers to a pending `AskUserQuestion` call, one answer per question
    /// in the order the questions were asked.
    pub async fn answer_question(
        &self,
        chat_id: ChatId,
        request_id: Uuid,
        answers: Vec<QuestionAnswer>,
    ) -> Result<(), AgentError> {
        let mut chats = self.inner.chats.lock().await;
        let chat = chats.get_mut(&chat_id).ok_or(AgentError::NotFound)?;
        let request = chat
            .snapshot
            .questions
            .iter()
            .find(|request| request.id == request_id)
            .cloned()
            .ok_or_else(|| AgentError::Invalid("question expired".into()))?;
        if answers.len() != request.questions.len() {
            return Err(AgentError::Invalid(format!(
                "answer each of the {} questions",
                request.questions.len()
            )));
        }
        for (question, answer) in request.questions.iter().zip(&answers) {
            answer.validate(question).map_err(AgentError::Invalid)?;
        }
        let result = answered_questions_result(&request.questions, &answers);
        if result.len() > MAX_ANSWER_BYTES {
            return Err(AgentError::Invalid(format!(
                "answers must fit in {MAX_ANSWER_BYTES} bytes"
            )));
        }
        if chat
            .run_cancel(request.run_id)
            .is_none_or(CancellationToken::is_cancelled)
            || !chat.snapshot.loops.iter().any(|goal| {
                goal.id == request.loop_id
                    && goal.run_id == Some(request.run_id)
                    && goal.state == LoopState::WaitingQuestion
            })
        {
            return Err(AgentError::Invalid("question was cancelled".into()));
        }
        let mut pending_answers = self.inner.question_answers.lock().await;
        if !pending_answers.contains_key(&request_id) {
            return Err(AgentError::Invalid("question was already answered".into()));
        }
        let mut snapshot = chat.snapshot.clone();
        snapshot
            .questions
            .retain(|request| request.id != request_id);
        if let Some(operation) = snapshot.operations.iter_mut().find(|operation| {
            operation.run_id == request.run_id && operation.call_id == request.call_id
        }) {
            operation.state = OperationState::Completed;
            operation.result = Some(ToolResult {
                content: result.clone(),
                is_error: false,
                output_ref: None,
            });
        }
        self.save(&snapshot);
        chat.snapshot = snapshot;
        pending_answers
            .remove(&request_id)
            .unwrap()
            .send(result)
            .map_err(|_| AgentError::Invalid("question expired".into()))
    }

    pub(super) async fn interaction_tool(
        &self,
        receipt: &Receipt,
        call_id: &str,
        name: &str,
        input: &Value,
        cancel: CancellationToken,
    ) -> Option<Result<ToolResult, String>> {
        match name {
            ASK_USER_QUESTION_TOOL_NAME => {
                Some(self.ask_question(receipt, call_id, input, cancel).await)
            }
            START_GOAL_TOOL_NAME => Some(self.start_goal(receipt, call_id, input, cancel).await),
            _ => None,
        }
    }

    async fn start_goal(
        &self,
        receipt: &Receipt,
        call_id: &str,
        input: &Value,
        cancel: CancellationToken,
    ) -> Result<ToolResult, String> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Input {
            goal: String,
        }
        let parsed: Input = serde_json::from_value(input.clone())
            .map_err(|_| format!("{START_GOAL_TOOL_NAME} requires only a goal string"))?;
        if parsed.goal.trim().is_empty() || parsed.goal.len() > MAX_ANSWER_BYTES {
            return Err("goal must contain 1 to 65536 bytes".into());
        }
        let mut chats = self.inner.chats.lock().await;
        let chat = chats
            .get_mut(&receipt.chat_id)
            .ok_or_else(|| "chat is unavailable".to_string())?;
        if cancel.is_cancelled()
            || chat.active_run != Some(receipt.run_id)
            || chat
                .active
                .as_ref()
                .is_none_or(CancellationToken::is_cancelled)
            || !chat.snapshot.loops.iter().any(|goal| {
                goal.id == receipt.loop_id
                    && goal.run_id == Some(receipt.run_id)
                    && goal.state == LoopState::Running
            })
        {
            return Err("run was cancelled before creating the new goal".into());
        }
        let mut snapshot = chat.snapshot.clone();
        let new_receipt = Receipt {
            chat_id: receipt.chat_id,
            loop_id: Uuid::new_v4(),
            run_id: Uuid::new_v4(),
        };
        snapshot.add_loop(LoopSnapshot {
            id: new_receipt.loop_id,
            goal: parsed.goal.clone(),
            state: LoopState::Queued,
            run_id: None,
            trigger: None,
            state_since: Some(crate::private_fs::unix_time_milliseconds()),
        });
        let index = snapshot
            .pending_inputs
            .iter()
            .position(|pending| pending.trigger_source.is_some())
            .unwrap_or(snapshot.pending_inputs.len());
        snapshot.last_queue_sequence += 1;
        let queued_sequence = snapshot.last_queue_sequence;
        snapshot.pending_inputs.insert(
            index,
            PendingInput {
                receipt: new_receipt.clone(),
                input: parsed.goal,
                contexts: Vec::new(),
                agent_message: None,
                denial_reference: None,
                trigger_source: None,
                armed: true,
                queued_sequence,
                task_continuation: false,
                is_skill_invocation: false,
                attached_file_reads: Vec::new(),
            },
        );
        let result = ToolResult {content: json!({"chat_id":new_receipt.chat_id,"loop_id":new_receipt.loop_id,"run_id":new_receipt.run_id,"state":"queued"}).to_string(), is_error: false, output_ref: None};
        if let Some(operation) = snapshot
            .operations
            .iter_mut()
            .find(|operation| operation.run_id == receipt.run_id && operation.call_id == call_id)
        {
            operation.state = OperationState::Completed;
            operation.result = Some(result.clone());
        }
        self.save(&snapshot);
        chat.snapshot = snapshot;
        Ok(result)
    }

    async fn ask_question(
        &self,
        receipt: &Receipt,
        call_id: &str,
        input: &Value,
        cancel: CancellationToken,
    ) -> Result<ToolResult, String> {
        let parsed: AskUserQuestionInput = serde_json::from_value(input.clone())
            .map_err(|error| format!("invalid {ASK_USER_QUESTION_TOOL_NAME} input: {error}"))?;
        if !has_unique_questions_and_labels(&parsed.questions) {
            return Err(UNIQUENESS_ERROR.into());
        }
        let request = QuestionRequest {
            id: Uuid::new_v4(),
            chat_id: receipt.chat_id,
            loop_id: receipt.loop_id,
            run_id: receipt.run_id,
            call_id: call_id.into(),
            questions: parsed.questions,
        };
        let (sender, receiver) = oneshot::channel();
        let next = {
            let mut chats = self.inner.chats.lock().await;
            let chat = chats
                .get_mut(&receipt.chat_id)
                .ok_or_else(|| "chat is unavailable".to_string())?;
            if cancel.is_cancelled()
                || chat.active_run != Some(receipt.run_id)
                || chat
                    .active
                    .as_ref()
                    .is_none_or(CancellationToken::is_cancelled)
            {
                return Err("run was cancelled before clarification".into());
            }
            let mut snapshot = chat.snapshot.clone();
            let goal = snapshot
                .loops
                .iter_mut()
                .find(|goal| goal.id == receipt.loop_id && goal.run_id == Some(receipt.run_id))
                .ok_or_else(|| "question belongs to an expired run".to_string())?;
            if goal.state != LoopState::Running {
                return Err("goal cannot ask a question in its current state".into());
            }
            goal.set_state(LoopState::WaitingQuestion);
            snapshot.questions.push(request.clone());
            self.save(&snapshot);
            chat.snapshot = snapshot;
            self.park_run(chat, receipt, cancel.clone())
                .map_err(|error| error.to_string())?;
            self.inner
                .question_answers
                .lock()
                .await
                .insert(request.id, sender);
            self.emit(
                chat,
                receipt,
                EventKind::QuestionAsked {
                    request: request.clone(),
                },
            );
            self.next_run(chat).map_err(|error| error.to_string())?
        };
        if let Some(next) = next {
            self.inner.tasks.lock().await.push(self.run_task(next));
        }
        let answer = tokio::select! {
            biased;
            _ = cancel.cancelled() => Err("question was cancelled".to_string()),
            answer = receiver => answer.map_err(|_| "question expired".to_string()),
        };
        let resumed = if answer.is_ok() {
            self.resume_parked_run(receipt, cancel.clone()).await
        } else {
            Ok(())
        };
        let mut chats = self.inner.chats.lock().await;
        let chat = chats
            .get_mut(&receipt.chat_id)
            .ok_or_else(|| "chat is unavailable".to_string())?;
        self.inner.question_answers.lock().await.remove(&request.id);
        let mut snapshot = chat.snapshot.clone();
        snapshot
            .questions
            .retain(|question| question.id != request.id);
        let accepted = snapshot
            .operations
            .iter()
            .find(|operation| {
                operation.run_id == receipt.run_id
                    && operation.call_id == call_id
                    && operation.state == OperationState::Completed
            })
            .and_then(|operation| operation.result.clone());
        self.save(&snapshot);
        chat.snapshot = snapshot;
        if let Some(result) = accepted {
            return Ok(result);
        }
        resumed?;
        answer.map(|content| ToolResult {
            content,
            is_error: false,
            output_ref: None,
        })
    }
}
