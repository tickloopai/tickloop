use super::*;
use crate::QuestionReply;
use serde_json::{Value, json};
use std::collections::HashSet;
use tickloop_agent_protocol::{
    EventKind, LoopSnapshot, LoopState, OperationState, PendingInput, Question, QuestionAnswer,
    QuestionRequest, Receipt, ToolDefinition, ToolResult,
};
use tickloop_agent_tools::{
    ASK_USER_QUESTION_TOOL_NAME, DECLINED_QUESTIONS_RESULT, EXIT_PLAN_MODE_TOOL_NAME,
    START_GOAL_TOOL_NAME,
};
use tickloop_agent_transcript::ModelVisibleResult;

const QUESTION_HEADER_MAX_CHARACTERS: usize = 12;
const MIN_QUESTIONS: usize = 1;
const MAX_QUESTIONS: usize = 4;
const MIN_QUESTION_OPTIONS: usize = 2;
const MAX_QUESTION_OPTIONS: usize = 4;
const MAX_ANSWER_BYTES: usize = 65536;
const UNIQUENESS_ERROR: &str =
    "Question texts must be unique, option labels must be unique within each question";
const QUESTION_SETTLED: &str = "question was already answered";

/// How the user settled a pending `AskUserQuestion` call, as its waiting Run receives it.
pub(crate) enum PendingReply {
    /// The tool result that reports the answers.
    Answered(String),
    /// Esc in the question dialog, as Claude Code's cancel: the call is rejected and the run
    /// ends there.
    Declined,
}

/// `answers`, `annotations` and `metadata` are accepted by the input schema for Claude Code
/// compatibility; its permission component fills them, so the host ignores them here.
#[derive(Deserialize)]
struct AskUserQuestionInput {
    questions: Vec<Question>,
}

/// Whether `answer` is a valid choice for `question`.
fn validate_answer(answer: &QuestionAnswer, question: &Question) -> Result<(), String> {
    let mut chosen = HashSet::new();
    for label in &answer.options {
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
    if answer
        .other
        .as_ref()
        .is_some_and(|text| text.trim().is_empty())
    {
        return Err("an \"Other\" answer needs text".into());
    }
    let choice_count = answer.options.len() + usize::from(answer.other.is_some());
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

fn pending_question(chat: &Chat, request_id: Uuid) -> Result<QuestionRequest, AgentError> {
    chat.snapshot
        .questions
        .iter()
        .find(|request| request.id == request_id)
        .cloned()
        .ok_or_else(|| AgentError::Invalid("question expired".into()))
}

/// Fails unless the run that asked `request` is still waiting for its reply.
fn ensure_question_waits(chat: &Chat, request: &QuestionRequest) -> Result<(), AgentError> {
    let run_is_live = !chat
        .run_cancel(request.run_id)
        .is_none_or(CancellationToken::is_cancelled);
    let loop_waits = chat.snapshot.loops.iter().any(|goal| {
        goal.id == request.loop_id
            && goal.run_id == Some(request.run_id)
            && goal.state == LoopState::WaitingQuestion
    });
    if run_is_live && loop_waits {
        Ok(())
    } else {
        Err(AgentError::Invalid("question was cancelled".into()))
    }
}

/// A question call's result as the conversation records it.
fn reported_result(text: String, ends_run: bool) -> ModelVisibleResult {
    ModelVisibleResult {
        tool: ToolResult {
            content: text.clone(),
            is_error: ends_run,
            output_ref: None,
        },
        content: text.into(),
        ends_run,
    }
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

/// Claude Code's `AskUserQuestion` prompt. Its plan mode note points at `ExitPlanMode`, which a
/// delegated child does not have, so a child's description leaves the note out.
fn ask_user_question_definition(is_delegated: bool) -> ToolDefinition {
    let plan_tool = EXIT_PLAN_MODE_TOOL_NAME;
    let plan_mode_note = if is_delegated {
        String::new()
    } else {
        format!(
            "
Plan mode note: In plan mode, use this tool to clarify requirements or choose between approaches BEFORE finalizing your plan. Do NOT use this tool to ask \"Is my plan ready?\" or \"Should I proceed?\" - use {plan_tool} for plan approval. IMPORTANT: Do not reference \"the plan\" in your questions (e.g., \"Do you have feedback about the plan?\", \"Does the plan look good?\") because the user cannot see the plan in the UI until you call {plan_tool}. If you need plan approval, use {plan_tool} instead.
"
        )
    };
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
{plan_mode_note}"
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

pub(super) fn definitions(is_delegated: bool) -> Vec<ToolDefinition> {
    vec![ask_user_question_definition(is_delegated), ToolDefinition {
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
        self.reply_to_question(chat_id, request_id, QuestionReply::Answered(answers), None)
            .await
            .map(|_| ())
    }

    /// Declines a pending `AskUserQuestion` call, as Esc does in Claude Code's question dialog:
    /// the call reports the rejection and its run ends there, like a declined permission. Input
    /// the user sends afterwards starts a new run.
    pub async fn decline_question(
        &self,
        chat_id: ChatId,
        request_id: Uuid,
    ) -> Result<(), AgentError> {
        self.reply_to_question(chat_id, request_id, QuestionReply::Declined, None)
            .await
            .map(|_| ())
    }

    /// Ends the wait of a pending question with `reply`. The result the call reports is decided
    /// now and recorded with the wait, with the ledger input `ledger` the reply came as, merged
    /// into the waiting Run; the Run puts the result on the chain once it holds the execution
    /// slot again. Gives the reply's receipt.
    pub(crate) async fn reply_to_question(
        &self,
        chat_id: ChatId,
        request_id: Uuid,
        reply: QuestionReply,
        ledger: Option<lifecycle::LedgerInput>,
    ) -> Result<Receipt, AgentError> {
        let mut chats = self.inner.chats.lock().await;
        let chat = chats.get_mut(&chat_id).ok_or(AgentError::NotFound)?;
        // A repeated reply, even one racing the first, finds it in the ledger under this lock.
        if let Some(replayed) = ledger
            .as_ref()
            .and_then(|input| chat.bookkeeping.lifecycle.replay(chat_id, input))
        {
            return replayed;
        }
        let request = pending_question(chat, request_id).map_err(|error| match ledger {
            Some(_) => AgentError::Invalid("the question is no longer pending".into()),
            None => error,
        })?;
        let (pending, result) = match reply {
            QuestionReply::Answered(answers) => {
                if answers.len() != request.questions.len() {
                    return Err(AgentError::Invalid(format!(
                        "answer each of the {} questions",
                        request.questions.len()
                    )));
                }
                for (question, answer) in request.questions.iter().zip(&answers) {
                    validate_answer(answer, question).map_err(AgentError::Invalid)?;
                }
                let text = answered_questions_result(&request.questions, &answers);
                if text.len() > MAX_ANSWER_BYTES {
                    return Err(AgentError::Invalid(format!(
                        "answers must fit in {MAX_ANSWER_BYTES} bytes"
                    )));
                }
                (
                    PendingReply::Answered(text.clone()),
                    reported_result(text, false),
                )
            }
            QuestionReply::Declined => (
                PendingReply::Declined,
                reported_result(DECLINED_QUESTIONS_RESULT.into(), true),
            ),
        };
        ensure_question_waits(chat, &request)?;
        let mut pending_answers = self.inner.question_answers.lock().await;
        let Some(waiting) = pending_answers.get(&request_id) else {
            return Err(AgentError::Invalid(QUESTION_SETTLED.into()));
        };
        // Nothing is recorded for a Run that no longer waits here, such as one just released.
        if waiting.is_closed() || Self::release_of(chat, request.run_id).is_cancelled() {
            return Err(AgentError::Invalid("question expired".into()));
        }
        let mut snapshot = chat.snapshot.clone();
        snapshot
            .questions
            .retain(|request| request.id != request_id);
        if let PendingReply::Answered(_) = &pending
            && let Some(operation) = snapshot.operations.iter_mut().find(|operation| {
                operation.run_id == request.run_id && operation.call_id == request.call_id
            })
        {
            operation.state = OperationState::Completed;
            operation.result = Some(result.tool.clone());
        }
        let receipt = Receipt {
            chat_id,
            loop_id: request.loop_id,
            run_id: Uuid::new_v4(),
        };
        let lifecycle = &mut chat.bookkeeping.lifecycle;
        lifecycle.resolve_wait(request_id, result, ledger.as_ref().map(|input| input.id));
        if let Some(input) = &ledger {
            lifecycle.record_merged(input, &receipt, request.run_id);
            self.inner.ledger_changes.send_replace(());
        }
        self.save(&snapshot);
        chat.snapshot = snapshot;
        pending_answers
            .remove(&request_id)
            .unwrap()
            .send(pending)
            .map_err(|_| AgentError::Invalid("question expired".into()))?;
        Ok(receipt)
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
        chat.bookkeeping.add_loop(
            &mut snapshot,
            LoopSnapshot {
                id: new_receipt.loop_id,
                goal: parsed.goal.clone(),
                state: LoopState::Queued,
                run_id: None,
                run_started_at: None,
                trigger: None,
                event_sources: Vec::new(),
                state_since: Some(tickloop_agent_base::private_fs::unix_time_milliseconds()),
            },
        );
        let index = snapshot
            .pending_inputs
            .iter()
            .position(|pending| pending.trigger_source.is_some())
            .unwrap_or(snapshot.pending_inputs.len());
        snapshot.last_queue_sequence += 1;
        let queued_sequence = snapshot.last_queue_sequence;
        let queued = PendingInput {
            receipt: new_receipt.clone(),
            input: parsed.goal,
            contexts: Vec::new(),
            agent_message: None,
            directed_human: None,
            denial_reference: None,
            trigger_source: None,
            armed: true,
            queued_sequence,
            task_continuation: false,
            is_skill_invocation: false,
            attached_file_reads: Vec::new(),
        };
        chat.bookkeeping.lifecycle.accept(
            &lifecycle::LedgerInput::new(
                tickloop_agent_transcript::input_ids::goal(new_receipt.loop_id),
                tickloop_agent_transcript::InputOp::Goal,
                &queued.input,
            ),
            &queued,
        );
        snapshot.pending_inputs.insert(index, queued);
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
        let release = {
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
            let release = self.suspend_for_question(chat, receipt, &request);
            self.park_run(chat, receipt, cancel.clone())
                .map_err(|error| error.to_string())?;
            self.save_now(&chat.snapshot);
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
            release
        };
        let next = self
            .next_run(receipt.chat_id)
            .await
            .map_err(|error| error.to_string())?;
        if let Some(next) = next {
            self.run_task(next);
        }
        let reply = tokio::select! {
            biased;
            _ = release.cancelled() => return Err(super::suspension::RUN_RELEASED.into()),
            _ = cancel.cancelled() => Err("question was cancelled".to_string()),
            reply = receiver => reply.map_err(|_| "question expired".to_string()),
        };
        let resumed = if reply.is_ok() {
            self.resume_parked_run(receipt, cancel.clone()).await
        } else {
            Ok(())
        };
        if release.is_cancelled() {
            return Err(super::suspension::RUN_RELEASED.into());
        }
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
        match reply? {
            PendingReply::Answered(content) => Ok(ToolResult {
                content,
                is_error: false,
                output_ref: None,
            }),
            // As a declined permission: the result reaches the conversation, then the run ends.
            PendingReply::Declined => {
                chat.user_cancelled_runs.insert(receipt.run_id);
                Ok(ToolResult {
                    content: DECLINED_QUESTIONS_RESULT.into(),
                    is_error: true,
                    output_ref: None,
                })
            }
        }
    }
}
