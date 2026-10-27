use crate::{
    Agent,
    codemode::{self, Control, Options},
    nested_calls::NestedStatus,
    tool_execution::{ToolCallError, ToolCallOutcome},
};
use serde_json::{Value, json};
use tickloop_harness_protocol::{
    OperationRecord, OperationState, Receipt, ToolDefinition, ToolResult,
};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

pub(crate) fn definition() -> ToolDefinition {
    ToolDefinition {
        name: "Codemode".into(),
        description: "Run JavaScript as an async function body in a QuickJS sandbox without files, network, timers or module loading. Use await tools.<name>(arguments), replacing invalid identifier characters with _. Exact tool names also work as tools['original-name']; exact names take precedence when normalized names collide. Each tool call has its usual permissions and may pause for confirmation. text(value), image(base64 data URL or MCP image block), console.log/info/warn/error/debug, return and exit() produce output; only explicit output returns to the model. First-line // @options: {\"max_output_tokens\":10000,\"timeout_ms\":300000} controls output and active elapsed time; confirmation waits do not count. Excess output preserves its start and end with a full-output file. Memory is limited to 256 MB. Tool errors and human denials throw Error; Auto denial ends the entire script. Calls still running at script completion are cancelled and may have an unknown outcome. Cannot call Codemode, ExecuteTools, AskUserQuestion, Subagent, Skill, plan mode controls, family control, scheduled task or Goal tools. Discover tool names and schemas with top-level SearchTools and DescribeTools.".into(),
        input_schema: json!({"type":"object","properties":{"code":{"type":"string"}},"required":["code"],"additionalProperties":false}),
    }
}
impl Agent {
    pub(crate) async fn execute_codemode(
        &self,
        receipt: &Receipt,
        message_id: Uuid,
        parent: String,
        input: Value,
        cancel: CancellationToken,
    ) -> Result<ToolCallOutcome, ToolCallError> {
        let Some(source) = input
            .get("code")
            .and_then(Value::as_str)
            .filter(|source| !source.trim().is_empty())
        else {
            return Ok(ToolCallOutcome::not_admitted(
                "code must be a nonempty JavaScript string".into(),
            ));
        };
        if input.as_object().is_none_or(|fields| fields.len() != 1) {
            return Ok(ToolCallOutcome::not_admitted(
                "Codemode accepts only code".into(),
            ));
        }
        let output_directory = {
            let mut chats = self.inner.chats.lock().await;
            let chat = chats.get_mut(&receipt.chat_id).ok_or("chat unavailable")?;
            chat.snapshot.operations.push(OperationRecord {
                parent_call_id: None,
                task_binding: None,
                id: tickloop_ids::generate(),
                loop_id: receipt.loop_id,
                run_id: receipt.run_id,
                call_id: parent.clone(),
                name: "Codemode".into(),
                input: input.clone(),
                file_change: None,
                state: OperationState::Admitted,
                result: None,
            });
            self.save(&chat.snapshot);
            chat.tools
                .family_temp_directory()
                .map(std::path::Path::to_owned)
        };
        let scope = self
            .nested_calls(receipt, message_id, parent.clone(), &cancel)
            .await?;
        let control = Control::default();
        scope.set_script_control(control.clone());
        let tools = scope.script_tools();
        let execution = codemode::run(
            source,
            Options {
                control: control.clone(),
                output_directory,
                ..Default::default()
            },
            tools,
            |name, arguments, cancel| {
                let scope = scope.clone();
                async move {
                    let outcome = scope.execute(name, arguments, cancel).await;
                    if outcome.status == NestedStatus::Completed {
                        Ok(outcome
                            .script_value
                            .unwrap_or(Value::String(outcome.result.content)))
                    } else {
                        Err(outcome.result.content)
                    }
                }
            },
        );
        tokio::pin!(execution);
        let stopped = scope.stop_token();
        let mut outcome = tokio::select! {
            biased;
            _ = stopped.cancelled() => { control.cancel.cancel(); let mut result = execution.await; result.error = Some("Script stopped after Auto rejection".into()); result },
            _ = cancel.cancelled() => { control.cancel.cancel(); execution.await },
            result = &mut execution => result,
        };
        scope.finish().await;
        if let Some(reason) = scope.rejection_reason() {
            outcome.error = Some(reason);
        }
        let mut content = if outcome.error.is_some() {
            "Script failed".to_owned()
        } else {
            "Script completed".to_owned()
        };
        if !outcome.output.is_empty() {
            content.push('\n');
            content.push_str(&outcome.output);
        }
        if let Some(path) = outcome.full_output_path {
            content.push_str(&format!("\nFull output saved to: {}", path.display()));
        }
        if let Some(error) = &outcome.error {
            content.push_str(&format!("\nScript error: {error}"));
        }
        let result = ToolResult {
            content,
            is_error: outcome.error.is_some(),
            output_ref: None,
        };
        {
            let mut chats = self.inner.chats.lock().await;
            let chat = chats.get_mut(&receipt.chat_id).ok_or("chat unavailable")?;
            if let Some(operation) =
                chat.snapshot.operations.iter_mut().find(|operation| {
                    operation.call_id == parent && operation.run_id == receipt.run_id
                })
            {
                operation.state = if result.is_error {
                    OperationState::Failed
                } else {
                    OperationState::Completed
                };
                operation.result = Some(result.clone());
            }
            self.save(&chat.snapshot);
        }
        Ok(ToolCallOutcome {
            result,
            images: std::mem::take(&mut outcome.images),
            ..ToolCallOutcome::not_admitted(String::new())
        })
    }
}
