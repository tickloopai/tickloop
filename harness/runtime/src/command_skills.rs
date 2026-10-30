//! Command preprocessing uses the same prepared Bash execution as an ordinary tool call.
use super::*;
use serde_json::Value;
use tickloop_harness_policy::skills::SkillExpansion;
use tickloop_harness_protocol::ProposedToolCall;

tokio::task_local! { static INPUT_MESSAGE: Uuid; }

pub(super) fn input_message_id() -> Option<Uuid> {
    INPUT_MESSAGE.try_with(|id| *id).ok()
}

#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub(super) enum UserSkillPreparation {
    Pending(Box<SkillExpansion>),
    Started,
    Complete,
}

pub(super) fn shell_input(name: &str, command: &str) -> Value {
    serde_json::json!({"command": command, "description": format!("Prepare /{name}"), "run_in_background": false})
}

pub(super) fn user_skill_prepared_without_shell(chat: &Chat, run: RunId) -> bool {
    chat.snapshot
        .user_skill_invocations
        .get(&run)
        .is_some_and(|invocation| invocation.prepared_without_shell)
}

impl Agent {
    pub(super) async fn skill_shell_output(
        &self,
        receipt: &Receipt,
        call_id: &str,
        result: &ToolResult,
    ) -> Result<String, String> {
        if result.is_error {
            return Err(format!("Command preprocessing failed: {}", result.content));
        }
        match self.command_state_for_call(receipt, call_id).await {
            Some(tickloop_harness_protocol::CommandState::Completed) => Ok(result.content.clone()),
            state => Err(format!(
                "Command preprocessing did not complete ({state:?}); it will not be sent again. {}",
                result.content
            )),
        }
    }

    pub(super) async fn prepare_user_skill(
        &self,
        receipt: &Receipt,
        cancel: &CancellationToken,
        is_awaiting_model: &mut bool,
    ) -> Result<bool, String> {
        let mut expansion = {
            let mut chats = self.inner.chats.lock().await;
            let chat = chats
                .get_mut(&receipt.chat_id)
                .ok_or("chat is unavailable")?;
            let is_skill_input = chat
                .snapshot
                .user_skill_invocations
                .contains_key(&receipt.run_id)
                || chat
                    .bookkeeping
                    .lifecycle
                    .open_input(receipt.run_id)
                    .and_then(|input| chat.bookkeeping.lifecycle.input(input))
                    .is_some_and(|input| {
                        matches!(input.op, tickloop_harness_transcript::InputOp::Skill)
                    });
            if !is_skill_input {
                return Ok(false);
            }
            if !chat.user_skill_preparations.contains_key(&receipt.run_id)
                && user_skill_prepared_without_shell(chat, receipt.run_id)
            {
                return Ok(false);
            }
            match chat.user_skill_preparations.insert(receipt.run_id, UserSkillPreparation::Started) {
                Some(UserSkillPreparation::Pending(expansion)) => expansion,
                Some(UserSkillPreparation::Complete) => {
                    chat.user_skill_preparations.insert(receipt.run_id, UserSkillPreparation::Complete);
                    return Ok(false);
                }
                _ => return Err("Skill preparation was interrupted or not retained; invoke the slash command again explicitly. No Shell was sent again.".into()),
            }
        };
        let mut outputs = Vec::new();
        for shell in &expansion.shell_commands {
            if cancel.is_cancelled() {
                return Err("Skill preparation cancelled before dispatch".into());
            }
            *is_awaiting_model = false;
            let registry = self.prepare_tool_registry(receipt, cancel).await?;
            let message_id = {
                let chats = self.inner.chats.lock().await;
                let chat = chats.get(&receipt.chat_id).ok_or("chat is unavailable")?;
                chat.snapshot
                    .message_runs
                    .iter()
                    .find_map(|(index, run)| {
                        (*run == receipt.run_id
                            && chat.snapshot.prompt_execution_modes.contains_key(index))
                        .then(|| chat.snapshot.message_ids.get(index).copied())
                        .flatten()
                    })
                    .ok_or("Skill input has no retained message identity")?
            };
            let call = ProposedToolCall {
                call_id: tickloop_ids::generate().to_string(),
                name: "Bash".into(),
                input: shell_input(&expansion.name, &shell.command),
            };
            let child = cancel.child_token();
            // No handover safe point inside this unit: the frozen plan is consumed once.
            let result = INPUT_MESSAGE
                .scope(message_id, async {
                    let outcome = self
                        .execute_registered_call(
                            receipt,
                            message_id,
                            ToolInvocation::new(
                                call.call_id.clone(),
                                call.name.clone(),
                                call.input.clone(),
                                registry.resolve(&call.name),
                            ),
                            child.clone(),
                        )
                        .await;
                    self.complete_input_call(receipt, &call, outcome, cancel)
                        .await
                })
                .await?;
            match self
                .skill_shell_output(receipt, &call.call_id, &result)
                .await
            {
                Ok(output) => outputs.push(output),
                Err(error) => {
                    child.cancel();
                    return Err(error);
                }
            }
        }
        expansion
            .complete_shells(&outputs)
            .map_err(|error| error.to_string())?;
        let mut invoked = tickloop_harness_protocol::InvokedSkill::try_from(*expansion)
            .map_err(|error| error.to_string())?;
        let mut chats = self.inner.chats.lock().await;
        let chat = chats
            .get_mut(&receipt.chat_id)
            .ok_or("chat is unavailable")?;
        if cancel.is_cancelled() {
            return Err("Skill preparation cancelled; guidance was not activated".into());
        }
        if invoked.source == tickloop_harness_protocol::SkillSource::BuiltIn
            && chat.skills.is_user_only(&invoked.name)
        {
            invoked.run_id = Some(receipt.run_id);
        }
        record_invoked_skill(chat, invoked);
        chat.user_skill_preparations
            .insert(receipt.run_id, UserSkillPreparation::Complete);
        self.save(&chat.snapshot);
        Ok(true)
    }
}
