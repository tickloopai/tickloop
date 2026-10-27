use tickloop_harness::protocol::ExecutionMode;

/// Reads an Execution Mode name as `--mode`, `--permission-mode`, `/mode` and `defaultMode` take
/// it, including Claude Code's names. A name with only a deprecated mapping returns that mode with
/// a warning; Claude Code's modes that Tickloop expresses as launch policies return an error naming
/// the replacement.
pub fn parse_execution_mode(value: &str) -> Result<(ExecutionMode, Option<&'static str>), String> {
    match value.trim() {
        "ask" | "acceptEdits" | "accept-edits" => Ok((ExecutionMode::AcceptEdits, None)),
        "auto" => Ok((ExecutionMode::Auto, None)),
        "fullAccess" | "full-access" => Ok((ExecutionMode::FullAccess, None)),
        "plan" => Ok((ExecutionMode::Plan, None)),
        "manual" => Ok((
            ExecutionMode::AcceptEdits,
            Some("`manual` was replaced by Ask; use `ask`"),
        )),
        "default" => Ok((
            ExecutionMode::AcceptEdits,
            Some("`default` is deprecated and maps to Ask; use `ask`"),
        )),
        "dontAsk" => Err(
            "`dontAsk` is not an execution mode in Tickloop; use --deny-on-prompt to refuse every permission prompt"
                .into(),
        ),
        name @ ("bypassPermissions" | "bypass") => Err(format!(
            "`{name}` is not an execution mode in Tickloop; use --dangerously-skip-permissions, which requires external isolation"
        )),
        name => Err(format!(
            "`{name}` is not an execution mode; choose acceptEdits, auto, plan or fullAccess"
        )),
    }
}
