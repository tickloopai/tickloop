//! Plan mode texts adapted from Claude Code's `EnterPlanModeTool`,
//! `ExitPlanModeTool` prompts and results and the plan mode reminders and
//! `plan_mode_exit` attachment of `utils/messages.ts`. Tickloop has no Explore or Plan agent
//! types: the planning reminder explores with Read, Glob and Grep, and delegates read-only
//! research or design to Subagent children, which report in their final reply.

use std::path::Path;

use crate::mcp_tools::ARTIFACT_EDIT_TOOL;

use tickloop_harness_protocol::SUBAGENT_TOOL_NAME;
use tickloop_harness_tools::{
    ASK_USER_QUESTION_TOOL_NAME, EDIT_TOOL_NAME, ENTER_PLAN_MODE_TOOL_NAME,
    EXIT_PLAN_MODE_TOOL_NAME, GLOB_TOOL_NAME, GREP_TOOL_NAME, READ_TOOL_NAME, WRITE_TOOL_NAME,
};

pub(super) const NOT_IN_PLAN_MODE: &str = "You are not in plan mode. This tool is only for exiting plan mode after writing a plan. If your plan was already approved, continue with implementation.";

/// Claude Code's subagent plan mode reminder (`getPlanModeV2SubAgentInstructions`) for a child
/// frozen in Plan. Tickloop gives the child no plan file: its final reply carries the findings or
/// plan to the parent, which submits the plan.
pub(super) const DELEGATED_PLAN_MODE_REMINDER: &str = "# Plan mode

Plan mode is active. The user indicated that they do not want you to execute yet -- you MUST NOT make any edits, run any non-readonly tools (including changing configs or making commits), or otherwise make any changes to the system. This supersedes any other instructions you have received (for example, to make edits). Your delegated authorization stays in Plan mode, and you do not submit plans for approval.

Instead, research the task with read-only tools (call only read-only MCP tools) and answer it comprehensively in your final reply: your findings, or the approach you recommend with the files to change, the existing code to reuse and how to verify it. Your parent Agent writes the plan and submits it for the user's approval. Use AskUserQuestion if you need to ask the user clarifying questions; if you do, ask all of them before proceeding.";

pub(super) fn enter_plan_mode_description() -> String {
    format!(
        "Use this tool proactively when you're about to start a non-trivial implementation task. Getting user sign-off on your approach before writing code prevents wasted effort and ensures alignment. This tool transitions you into plan mode where you can explore the codebase and design an implementation approach for user approval.

## When to Use This Tool

**Prefer using {ENTER_PLAN_MODE_TOOL_NAME}** for implementation tasks unless they're simple. Use it when ANY of these conditions apply:

1. **New Feature Implementation**: Adding meaningful new functionality
   - Example: \"Add a logout button\" - where should it go? What should happen on click?
   - Example: \"Add form validation\" - what rules? What error messages?

2. **Multiple Valid Approaches**: The task can be solved in several different ways
   - Example: \"Add caching to the API\" - could use Redis, in-memory, file-based, etc.
   - Example: \"Improve performance\" - many optimization strategies possible

3. **Code Modifications**: Changes that affect existing behavior or structure
   - Example: \"Update the login flow\" - what exactly should change?
   - Example: \"Refactor this component\" - what's the target architecture?

4. **Architectural Decisions**: The task requires choosing between patterns or technologies
   - Example: \"Add real-time updates\" - WebSockets vs SSE vs polling
   - Example: \"Implement state management\" - Redux vs Context vs custom solution

5. **Multi-File Changes**: The task will likely touch more than 2-3 files
   - Example: \"Refactor the authentication system\"
   - Example: \"Add a new API endpoint with tests\"

6. **Unclear Requirements**: You need to explore before understanding the full scope
   - Example: \"Make the app faster\" - need to profile and identify bottlenecks
   - Example: \"Fix the bug in checkout\" - need to investigate root cause

7. **User Preferences Matter**: The implementation could reasonably go multiple ways
   - If you would use {ASK_USER_QUESTION_TOOL_NAME} to clarify the approach, use {ENTER_PLAN_MODE_TOOL_NAME} instead
   - Plan mode lets you explore first, then present options with context

## When NOT to Use This Tool

Only skip {ENTER_PLAN_MODE_TOOL_NAME} for simple tasks:
- Single-line or few-line fixes (typos, obvious bugs, small tweaks)
- Adding a single function with clear requirements
- Tasks where the user has given very specific, detailed instructions
- Pure research/exploration tasks

## What Happens in Plan Mode

In plan mode, you'll:
1. Thoroughly explore the codebase using {GLOB_TOOL_NAME}, {GREP_TOOL_NAME}, and {READ_TOOL_NAME} tools
2. Understand existing patterns and architecture
3. Design an implementation approach
4. Present your plan to the user for approval
5. Use {ASK_USER_QUESTION_TOOL_NAME} if you need to clarify approaches
6. Exit plan mode with {EXIT_PLAN_MODE_TOOL_NAME} when ready to implement

## Examples

### GOOD - Use {ENTER_PLAN_MODE_TOOL_NAME}:
User: \"Add user authentication to the app\"
- Requires architectural decisions (session vs JWT, where to store tokens, middleware structure)

User: \"Optimize the database queries\"
- Multiple approaches possible, need to profile first, significant impact

User: \"Implement dark mode\"
- Architectural decision on theme system, affects many components

User: \"Add a delete button to the user profile\"
- Seems simple but involves: where to place it, confirmation dialog, API call, error handling, state updates

User: \"Update the error handling in the API\"
- Affects multiple files, user should approve the approach

### BAD - Don't use {ENTER_PLAN_MODE_TOOL_NAME}:
User: \"Fix the typo in the README\"
- Straightforward, no planning needed

User: \"Add a console.log to debug this function\"
- Simple, obvious implementation

User: \"What files handle routing?\"
- Research task, not implementation planning

## Important Notes

- This tool REQUIRES user approval - they must consent to entering plan mode
- If unsure whether to use it, err on the side of planning - it's better to get alignment upfront than to redo work
- Users appreciate being consulted before significant changes are made to their codebase
"
    )
}

pub(super) fn exit_plan_mode_description() -> String {
    format!(
        "Use this tool when you are in plan mode and have finished writing your plan to the plan file and are ready for user approval.

## How This Tool Works
- You should have already written your plan to the plan file specified in the plan mode system message
- This tool does NOT take the plan content as a parameter - it will read the plan from the file you wrote
- This tool simply signals that you're done planning and ready for the user to review and approve
- The user will see the contents of your plan file when they review it

## When to Use This Tool
IMPORTANT: Only use this tool when the task requires planning the implementation steps of a task that requires writing code. For research tasks where you're gathering information, searching files, reading files or in general trying to understand the codebase - do NOT use this tool.

## Before Using This Tool
Ensure your plan is complete and unambiguous:
- If you have unresolved questions about requirements or approach, use {ASK_USER_QUESTION_TOOL_NAME} first (in earlier phases)
- Once your plan is finalized, use THIS tool to request approval

**Important:** Do NOT use {ASK_USER_QUESTION_TOOL_NAME} to ask \"Is this plan okay?\" or \"Should I proceed?\" - that's exactly what THIS tool does. {EXIT_PLAN_MODE_TOOL_NAME} inherently requests user approval of your plan.

## Examples

1. Initial task: \"Search for and understand the implementation of vim mode in the codebase\" - Do not use the exit plan mode tool because you are not planning the implementation steps of a task.
2. Initial task: \"Help me implement yank mode for vim\" - Use the exit plan mode tool after you have finished planning the implementation steps of the task.
3. Initial task: \"Add a new feature to handle user authentication\" - If unsure about auth method (OAuth, JWT, etc.), use {ASK_USER_QUESTION_TOOL_NAME} first, then use exit plan mode tool after clarifying the approach.
"
    )
}

/// Claude Code's result for an approved `EnterPlanMode`. Its closing line forbids every edit
/// because the plan file exception arrives in the plan mode reminder; here it names the exception.
pub(super) fn entered_plan_mode() -> String {
    format!(
        "Entered plan mode. You should now focus on exploring the codebase and designing an implementation approach.

In plan mode, you should:
1. Thoroughly explore the codebase to understand existing patterns
2. Identify similar features and architectural approaches
3. Consider multiple approaches and their trade-offs
4. Use {ASK_USER_QUESTION_TOOL_NAME} if you need to clarify the approach
5. Design a concrete implementation strategy
6. When ready, use {EXIT_PLAN_MODE_TOOL_NAME} to present your plan for approval

Remember: DO NOT write or edit any files except the plan file. {ARTIFACT_EDIT_TOOL} may also create or revise Artifact documents, subject to the host's permission checks. Stay in Plan mode for this; the host handles the first-publication confirmation. This is an exploration and planning phase."
    )
}

/// The result of an `EnterPlanMode` call the user refused.
pub(crate) fn enter_plan_mode_declined(comment: Option<&str>) -> String {
    match comment {
        Some(comment) => {
            format!("User declined to enter plan mode and gave this reason: {comment}")
        }
        None => "User declined to enter plan mode.".into(),
    }
}

pub(super) fn already_in_plan_mode() -> String {
    format!(
        "You are already in plan mode. Continue exploring and designing your approach, write your plan to the plan file, and use {EXIT_PLAN_MODE_TOOL_NAME} when it is ready for approval."
    )
}

pub(super) fn missing_plan_file(path: &Path) -> String {
    format!(
        "No plan file found at {}. Please write your plan to this file before calling {EXIT_PLAN_MODE_TOOL_NAME}.",
        path.display()
    )
}

pub(super) fn empty_plan_file(path: &Path) -> String {
    format!(
        "The plan file at {} is empty. Please write your plan to this file before calling {EXIT_PLAN_MODE_TOOL_NAME}.",
        path.display()
    )
}

pub(super) fn declined_plan(feedback: Option<&str>) -> String {
    match feedback {
        Some(feedback) => format!(
            "User has requested changes to your plan. Keep planning, update the plan file, and call ExitPlanMode again when ready.\n\nUser feedback: {feedback}"
        ),
        None => {
            "User cancelled plan approval. Remain in plan mode and wait for further instructions."
                .into()
        }
    }
}

/// The approved ExitPlanMode result, also used to continue a saved plan whose original Run ended.
pub(super) fn approved_plan(path: &Path, markdown: &str) -> String {
    format!(
        "User has approved your plan. You can now start coding. Proceed with the approved work

Your plan has been saved to: {}
You can refer back to it if needed during implementation.

## Approved Plan:
{markdown}",
        path.display()
    )
}

/// Claude Code's `plan_mode_exit` attachment, sent once after the user leaves Plan mode without
/// approving a plan.
pub(crate) fn plan_mode_exit_notice(plan_file: Option<&Path>) -> String {
    let plan_reference = plan_file
        .map(|path| {
            format!(
                " The plan file is located at {} if you need to reference it.",
                path.display()
            )
        })
        .unwrap_or_default();
    format!(
        "## Exited Plan Mode

You have exited plan mode. You can now make edits, run tools, and take actions.{plan_reference}"
    )
}

/// Claude Code's `plan_mode_reentry` attachment, ahead of the full reminder when the user returns
/// to Plan mode after leaving it and the plan file from that planning remains.
pub(crate) fn plan_mode_reentry(plan_file: &Path) -> String {
    format!(
        "## Re-entering Plan Mode

You are returning to plan mode after having previously exited it. A plan file exists at {} from your previous planning session.

**Before proceeding with any new planning, you should:**
1. Read the existing plan file to understand what was previously planned
2. Evaluate the user's current request against that plan
3. Decide how to proceed:
   - **Different task**: If the user's request is for a different task\u{2014}even if it's similar or related\u{2014}start fresh by overwriting the existing plan
   - **Same task, continuing**: If this is explicitly a continuation or refinement of the exact same task, modify the existing plan while cleaning up outdated or irrelevant sections
4. Continue on with the plan process and most importantly you should always edit the plan file one way or the other before calling {EXIT_PLAN_MODE_TOOL_NAME}

Treat this as a fresh planning session. Do not assume the existing plan is relevant without evaluating it first.",
        plan_file.display()
    )
}

/// Claude Code's plan mode reminder for a session without a plan file, for a Chat without a
/// Folder in Plan mode.
const PLAN_MODE_WITHOUT_PLAN_FILE_REMINDER: &str = "# Plan mode

Plan mode is active. The user indicated that they do not want you to execute yet -- you MUST NOT make any edits, run any non-readonly tools (including changing configs or making commits), or otherwise make any changes to the system. This supersedes any other instructions you have received. There is no plan file: give your plan to the user as text in your reply.";

/// Claude Code's sparse plan mode reminder, repeated every few prompts between full ones.
fn sparse_plan_mode_reminder(plan_file: &Path) -> String {
    format!(
        "Plan mode still active (see full instructions earlier in conversation). Read-only except plan file ({}) and Artifact documents through {ARTIFACT_EDIT_TOOL}. Follow the iterative planning workflow, and end your turn only with {ASK_USER_QUESTION_TOOL_NAME} or {EXIT_PLAN_MODE_TOOL_NAME}.",
        plan_file.display()
    )
}

/// Plan's rules as a mode note carries them: a delegated child, which has no plan file,
/// researches read-only and reports in its final reply; a root Chat hears the full reminder
/// naming its plan file, or the sparse one between full reminders.
pub(crate) fn plan_mode_rules(
    is_delegated: bool,
    plan_file: Option<&Path>,
    is_full: bool,
) -> String {
    match plan_file {
        _ if is_delegated => DELEGATED_PLAN_MODE_REMINDER.into(),
        None => PLAN_MODE_WITHOUT_PLAN_FILE_REMINDER.into(),
        Some(plan_file) if is_full => plan_mode_reminder(plan_file, plan_file.exists()),
        Some(plan_file) => sparse_plan_mode_reminder(plan_file),
    }
}

/// The full plan mode reminder a root Chat with a Folder hears on entering Plan mode and every
/// few sparse reminders after.
pub(crate) fn plan_mode_reminder(plan_file: &Path, plan_exists: bool) -> String {
    let plan_file = plan_file.display();
    let plan_file_info = if plan_exists {
        format!(
            "A plan file already exists at {plan_file}. You can read it and make incremental edits using the {EDIT_TOOL_NAME} tool."
        )
    } else {
        format!(
            "No plan file exists yet. You should create your plan at {plan_file} using the {WRITE_TOOL_NAME} tool."
        )
    };
    format!(
        "# Plan mode

Plan mode is active. The user indicated that they do not want you to execute yet -- you MUST NOT make any edits (except the plan file mentioned below and Artifact documents through {ARTIFACT_EDIT_TOOL}), run other non-readonly tools (including changing configs or making commits), or otherwise make any changes to the system. {ARTIFACT_EDIT_TOOL} may create or revise an Artifact while staying in Plan mode. The host handles the first-publication confirmation and checks the current audience and permission rules before each write. This supersedes other instructions to implement changes.

## Plan File Info:
{plan_file_info}
You should build your plan incrementally by writing to or editing this file. This is the only filesystem file you may edit. You may also present your research or plan in an Artifact through {ARTIFACT_EDIT_TOOL}, subject to the host's permission checks. All other actions must be READ-ONLY; call only read-only MCP tools.

## Iterative Planning Workflow

You are pair-planning with the user. Explore the code to build context, ask the user questions when you hit decisions you can't make alone, and write your findings into the plan file as you go. The plan file (above) is the ONLY file you may edit — it starts as a rough skeleton and gradually becomes the final plan.

### The Loop

Repeat this cycle until the plan is complete:

1. **Explore** — Use {READ_TOOL_NAME}, {GLOB_TOOL_NAME}, {GREP_TOOL_NAME} to read code. Look for existing functions, utilities, and patterns to reuse. You can use {SUBAGENT_TOOL_NAME} to parallelize complex searches or to design an approach without filling your context, though for straightforward queries direct tools are simpler. Children you create while planning stay read-only and return their findings or proposed design in their final reply; only you write the plan file and call {EXIT_PLAN_MODE_TOOL_NAME}.
2. **Update the plan file** — After each discovery, immediately capture what you learned. Don't wait until the end.
3. **Ask the user** — When you hit an ambiguity or decision you can't resolve from code alone, use {ASK_USER_QUESTION_TOOL_NAME}. Then go back to step 1.

### First Turn

Start by quickly scanning a few key files to form an initial understanding of the task scope. Then write a skeleton plan (headers and rough notes) and ask the user your first round of questions. Don't explore exhaustively before engaging the user.

### Asking Good Questions

- Never ask what you could find out by reading the code
- Batch related questions together (use multi-question {ASK_USER_QUESTION_TOOL_NAME} calls)
- Focus on things only the user can answer: requirements, preferences, tradeoffs, edge case priorities
- Scale depth to the task — a vague feature request needs many rounds; a focused bug fix may need one or none

### Plan File Structure
Your plan file should be divided into clear sections using markdown headers, based on the request. Fill out these sections as you go.
- Begin with a **Context** section: explain why this change is being made — the problem or need it addresses, what prompted it, and the intended outcome
- Include only your recommended approach, not all alternatives
- Ensure that the plan file is concise enough to scan quickly, but detailed enough to execute effectively
- Include the paths of critical files to be modified
- Reference existing functions and utilities you found that should be reused, with their file paths
- Include a verification section describing how to test the changes end-to-end (run the code, use MCP tools, run tests)

### When to Converge

Your plan is ready when you've addressed all ambiguities and it covers: what to change, which files to modify, what existing code to reuse (with file paths), and how to verify the changes. Call {EXIT_PLAN_MODE_TOOL_NAME} when the plan is ready for approval.

### Ending Your Turn

Your turn should only end by either:
- Using {ASK_USER_QUESTION_TOOL_NAME} to gather more information
- Calling {EXIT_PLAN_MODE_TOOL_NAME} when the plan is ready for approval

**Important:** Use {EXIT_PLAN_MODE_TOOL_NAME} to request plan approval. Do NOT ask about plan approval via text or {ASK_USER_QUESTION_TOOL_NAME}."
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_reminder_names_the_plan_file_and_how_to_finish() {
        let path = Path::new("/root/plans/calm-baking-aurora-loop-x.md");
        let missing = plan_mode_reminder(path, false);
        assert!(missing.contains(
            "You should create your plan at /root/plans/calm-baking-aurora-loop-x.md using the Write tool."
        ));
        let existing = plan_mode_reminder(path, true);
        assert!(existing.contains(
            "A plan file already exists at /root/plans/calm-baking-aurora-loop-x.md. You can read it and make incremental edits using the Edit tool."
        ));
        for reminder in [missing, existing] {
            assert!(reminder.contains("This is the only filesystem file you may edit."));
            assert!(reminder.contains("call only read-only MCP tools."));
            assert!(reminder.contains(
                "ArtifactEdit may create or revise an Artifact while staying in Plan mode."
            ));
            assert!(
                reminder.contains("- Calling ExitPlanMode when the plan is ready for approval")
            );
            assert!(reminder.contains(
                "Children you create while planning stay read-only and return their findings or proposed design in their final reply; only you write the plan file and call ExitPlanMode."
            ));
        }
    }

    #[test]
    fn plan_rules_follow_the_chat_and_the_reminder_kind() {
        let path = Path::new("/root/plans/calm-baking-aurora-loop-x.md");
        assert_eq!(
            plan_mode_rules(true, None, true),
            DELEGATED_PLAN_MODE_REMINDER
        );
        assert_eq!(
            plan_mode_rules(false, None, false),
            PLAN_MODE_WITHOUT_PLAN_FILE_REMINDER
        );
        assert_eq!(
            plan_mode_rules(false, Some(path), true),
            plan_mode_reminder(path, false)
        );
        let sparse = plan_mode_rules(false, Some(path), false);
        assert!(sparse.starts_with("Plan mode still active (see full instructions earlier in conversation). Read-only except plan file (/root/plans/calm-baking-aurora-loop-x.md)"));
        assert!(sparse.ends_with("end your turn only with AskUserQuestion or ExitPlanMode."));
    }

    #[test]
    fn a_delegated_child_reports_its_plan_instead_of_submitting_it() {
        assert!(DELEGATED_PLAN_MODE_REMINDER.contains("you MUST NOT make any edits"));
        assert!(DELEGATED_PLAN_MODE_REMINDER.contains("in your final reply"));
        assert!(DELEGATED_PLAN_MODE_REMINDER.contains("call only read-only MCP tools"));
        assert!(!DELEGATED_PLAN_MODE_REMINDER.contains(EXIT_PLAN_MODE_TOOL_NAME));
        assert!(!DELEGATED_PLAN_MODE_REMINDER.contains("plan file"));
    }

    #[test]
    fn the_exit_notice_references_an_existing_plan_file() {
        let notice = "## Exited Plan Mode\n\nYou have exited plan mode. You can now make edits, run tools, and take actions.";
        assert_eq!(plan_mode_exit_notice(None), notice);
        assert_eq!(
            plan_mode_exit_notice(Some(Path::new("/root/plans/calm-baking-aurora-loop-x.md"))),
            format!(
                "{notice} The plan file is located at /root/plans/calm-baking-aurora-loop-x.md if you need to reference it."
            )
        );
    }
}
