//! Plan mode texts adapted from Claude Code 2.1.302's `EnterPlanMode` and `ExitPlanMode` prompts
//! and results and its `plan_mode`, `plan_mode_reentry` and `plan_mode_exit` attachments.
//! Tickloop has no Explore or Plan agent types: the planning reminder delegates read-only
//! research or design to Subagent children, which report in their final reply.

use std::path::Path;

use crate::mcp_tools::ARTIFACT_EDIT_TOOL;

use tickloop_harness_protocol::{Plan, SUBAGENT_TOOL_NAME};
use tickloop_harness_tools::{
    ASK_USER_QUESTION_TOOL_NAME, EDIT_TOOL_NAME, ENTER_PLAN_MODE_TOOL_NAME,
    EXIT_PLAN_MODE_TOOL_NAME, GLOB_TOOL_NAME, GREP_TOOL_NAME, READ_TOOL_NAME, WRITE_TOOL_NAME,
};

/// Claude Code 2.1.302's `ExitPlanMode` refusal outside Plan mode.
pub(super) const NOT_IN_PLAN_MODE: &str = "You are not in plan mode. To enter plan mode, call the EnterPlanMode tool first. If your plan was already approved, continue with implementation.";

/// Claude Code 2.1.302's default counts of Explore and Plan agents (`VYn`, `KYn`), here the
/// Subagent children the planning workflow launches.
const EXPLORE_SUBAGENTS: usize = 3;
const DESIGN_SUBAGENTS: usize = 1;

/// Claude Code 2.1.302's Plan mode reminder for a subagent without a plan file (`Wbs`), for a
/// child frozen in Plan: its final reply carries the findings or plan to the parent, which
/// submits the plan. Tickloop adds the read-only MCP rule.
pub(super) fn delegated_plan_mode_reminder() -> String {
    format!(
        "{} Answer the query you were given comprehensively in your final message, and do not write a plan or anything else to a file. Call only read-only MCP tools.",
        plan_mode_is_active("")
    )
}

/// Claude Code 2.1.302's `EnterPlanMode` prompt. Its "Pure research/exploration tasks" item
/// points to the Agent tool by default; here to Subagent, when the Chat offers it.
pub(super) fn enter_plan_mode_description(offers_subagent: bool) -> String {
    let research_note = if offers_subagent {
        format!(" (use the {SUBAGENT_TOOL_NAME} tool instead)")
    } else {
        String::new()
    };
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
- Pure research/exploration tasks{research_note}

## What Happens in Plan Mode

In plan mode, you'll:
1. Thoroughly explore the codebase using {GLOB_TOOL_NAME}, {GREP_TOOL_NAME}, and {READ_TOOL_NAME}
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

/// Claude Code 2.1.302's `ExitPlanMode` prompt.
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

/// Claude Code 2.1.302's `EnterPlanMode` result. Its closing line forbids every edit; the plan
/// file and Artifact exceptions arrive with the Plan mode reminder of the next request.
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

Remember: DO NOT write or edit any files yet. This is a read-only exploration and planning phase."
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

/// Claude Code 2.1.302's approved ExitPlanMode result, also used to continue a saved plan whose
/// original Run ended. It suggests updating the task list only when the Chat offers the task
/// tools, marks a plan the user edited in review and appends feedback given on approval.
pub(super) fn approved_plan(
    path: &Path,
    plan: &Plan,
    feedback: Option<&str>,
    offers_task_tools: bool,
) -> String {
    let task_list_hint = if offers_task_tools {
        " Start with updating your todo list if applicable"
    } else {
        ""
    };
    let heading = if plan.edited_by_user {
        "Approved Plan (edited by user)"
    } else {
        "Approved Plan"
    };
    let feedback = feedback
        .map(|feedback| format!("\n\nUser feedback on this plan: {feedback}"))
        .unwrap_or_default();
    format!(
        "User has approved your plan. You can now start coding.{task_list_hint}

Your plan has been saved to: {}
You can refer back to it if needed during implementation.

## {heading}:
{}{feedback}",
        path.display(),
        plan.markdown
    )
}

/// Claude Code 2.1.302's ExitPlanMode result when the user lets the model leave Plan mode
/// without a plan.
pub(super) const APPROVED_PLAN_MODE_EXIT: &str =
    "User has approved exiting plan mode. You can now proceed.";

/// The result when the user keeps the model in Plan mode instead of letting it leave without a
/// plan.
pub(super) fn declined_plan_mode_exit(feedback: Option<&str>) -> String {
    match feedback {
        Some(feedback) => format!(
            "User declined to exit plan mode. Stay in plan mode and keep planning.\n\nUser feedback: {feedback}"
        ),
        None => "User declined to exit plan mode. Remain in plan mode and wait for further instructions."
            .into(),
    }
}

/// Claude Code's `plan_mode_exit` attachment, sent once after the Chat leaves Plan mode.
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

/// Claude Code 2.1.302's `wct`: the opening every Plan mode reminder and refusal shares.
fn plan_mode_is_active(exception: &str) -> String {
    format!(
        "Plan mode is active. The user indicated that they do not want you to execute yet -- you MUST NOT make any edits{exception}, run any non-readonly tools (including changing configs or making commits), or otherwise make any changes to the system. This supercedes any other instructions you have received."
    )
}

/// Phases 1 and 2 of Claude Code 2.1.302's planning workflow (`a6n`).
fn planning_phases(offers_subagent: bool) -> (String, String) {
    if offers_subagent {
        (
            format!(
                "### Phase 1: Initial Understanding
Goal: Gain a comprehensive understanding of the user's request by reading through code and asking them questions. Critical: In this phase you should only use {SUBAGENT_TOOL_NAME} children for read-only exploration.

1. Focus on understanding the user's request and the code associated with their request. Actively search for existing functions, utilities, and patterns that can be reused — avoid proposing new code when suitable implementations already exist.

2. **Launch up to {EXPLORE_SUBAGENTS} {SUBAGENT_TOOL_NAME} children IN PARALLEL** (single message, multiple tool calls) to efficiently explore the codebase.
   - Use 1 agent when the task is isolated to known files, the user provided specific file paths, or you're making a small targeted change.
   - Use multiple agents when: the scope is uncertain, multiple areas of the codebase are involved, or you need to understand existing patterns before planning.
   - Quality over quantity - {EXPLORE_SUBAGENTS} agents maximum, but you should try to use the minimum number of agents necessary (usually just 1)
   - If using multiple agents: Provide each agent with a specific search focus or area to explore. Example: One agent searches for existing implementations, another explores related components, a third investigating testing patterns"
            ),
            format!(
                "### Phase 2: Design
Goal: Design an implementation approach.

Launch {SUBAGENT_TOOL_NAME} children to design the implementation based on the user's intent and your exploration results from Phase 1.

You can launch up to {DESIGN_SUBAGENTS} agent(s) in parallel.

**Guidelines:**
- **Default**: Launch at least 1 {SUBAGENT_TOOL_NAME} child for most tasks - it helps validate your understanding and consider alternatives
- **Skip agents**: Only for truly trivial tasks (typo fixes, single-line changes, simple renames)

In the agent prompt:
- Provide comprehensive background context from Phase 1 exploration including filenames and code path traces
- Describe requirements and constraints
- Request a detailed implementation plan"
            ),
        )
    } else {
        (
            "### Phase 1: Initial Understanding
Goal: Gain a comprehensive understanding of the user's request by reading through code and asking them questions.

1. Focus on understanding the user's request and the code associated with their request. Actively search for existing functions, utilities, and patterns that can be reused — avoid proposing new code when suitable implementations already exist.

2. Read and explore the relevant files directly to efficiently understand the codebase."
                .to_owned(),
            "### Phase 2: Design
Goal: Design an implementation approach based on the user's intent and your exploration results from Phase 1.

- Provide comprehensive background context from Phase 1 exploration including filenames and code path traces
- Describe requirements and constraints
- Produce a detailed implementation plan"
                .to_owned(),
        )
    }
}

/// Phase 3 of the planning workflow (`d6n`).
fn review_phase() -> String {
    format!(
        "### Phase 3: Review
Goal: Review the plan(s) from Phase 2 and ensure alignment with the user's intentions.
1. Read the critical files you identified during exploration to deepen your understanding
2. Ensure that the plans align with the user's original request
3. Use {ASK_USER_QUESTION_TOOL_NAME} to clarify any remaining questions with the user"
    )
}

/// What the final plan holds (`s6n`), where `plan` names the plan file or the reply's plan.
fn final_plan_items(plan: &str) -> String {
    format!(
        "- Begin with a **Context** section: explain why this change is being made — the problem or need it addresses, what prompted it, and the intended outcome
- Include only your recommended approach, not all alternatives
- Ensure that the {plan} is concise enough to scan quickly, but detailed enough to execute effectively
- Name the critical files to be modified. For changes that repeat a pattern across many files, describe the pattern once and list a few representative paths — do not enumerate every file or line number
- Reference existing functions and utilities you found that should be reused, with their file paths
- Include a verification section describing how to test the changes end-to-end (run the code, use MCP tools, run tests)"
    )
}

/// The workflow's closing note (`l6n`).
fn questions_note() -> String {
    format!(
        "NOTE: At any point in time through this workflow you should feel free to ask the user questions or clarifications using the {ASK_USER_QUESTION_TOOL_NAME} tool. Don't make large assumptions about user intent. The goal is to present a well researched plan to the user, and tie any loose ends before implementation begins."
    )
}

/// Claude Code 2.1.302's full `plan_mode` attachment without a plan file (`Wbs`), for a Chat
/// without a Folder: the plan goes in the reply, and only the user leaves Plan.
pub(crate) fn folderless_plan_mode_reminder(offers_subagent: bool) -> String {
    let (phase1, phase2) = planning_phases(offers_subagent);
    format!(
        "{}

## Where Your Plan Goes:
This session has no plan file. Give your plan to the user as text in your reply, and do not write it to a file.

## Plan Workflow

{phase1}

{phase2}

{}

### Phase 4: Final Plan
Goal: Give your final plan in your reply.
{}

### Phase 5: Hand Over To The User
When your plan is final, end your turn with it. You cannot leave plan mode yourself, and the user's approval alone does not end it: to have the plan carried out, the user changes this session's permission mode from Plan to another mode in their app and then tells you to go ahead. Close your reply with one sentence that tells the user to do that. If they ask for changes instead, reply with the revised plan. Stay read-only until a system reminder says that you have exited plan mode. If they tell you to go ahead before that, tell them that the session is still in plan mode and how to change it.

{}",
        plan_mode_is_active(""),
        review_phase(),
        final_plan_items("plan"),
        questions_note()
    )
}

/// Claude Code 2.1.302's sparse `plan_mode` attachment without a plan file.
pub(crate) const SPARSE_FOLDERLESS_PLAN_MODE_REMINDER: &str = "Plan mode still active (see full instructions earlier in conversation). Read-only, and there is no plan file. Follow 5-phase workflow. When the plan is final, give it in your reply. Only the user can end plan mode, by changing the session's permission mode.";

/// Claude Code 2.1.302's full `plan_mode` attachment (`Bbs`) with its five-phase workflow. With
/// Subagent, the phases that launch Explore and Plan agents launch read-only Subagent children,
/// at Claude Code's default counts; without it, the phases explore directly. Tickloop adds the
/// Artifact exception and the read-only MCP rule.
pub(crate) fn plan_mode_reminder(
    plan_file: &Path,
    plan_exists: bool,
    offers_subagent: bool,
) -> String {
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
    let opening = plan_mode_is_active(&format!(
        " (with the exception of the plan file mentioned below and Artifact documents through {ARTIFACT_EDIT_TOOL})"
    ));
    let (phase1, phase2) = planning_phases(offers_subagent);
    let (review, final_plan, questions) = (
        review_phase(),
        final_plan_items("plan file"),
        questions_note(),
    );
    let children_note = if offers_subagent {
        format!(
            " {SUBAGENT_TOOL_NAME} children you create while planning stay read-only and return their findings or proposed design in their final reply; only you write the plan file and call {EXIT_PLAN_MODE_TOOL_NAME}."
        )
    } else {
        String::new()
    };
    format!(
        "{opening} {ARTIFACT_EDIT_TOOL} may create or revise an Artifact while staying in Plan mode. The host handles the first-publication confirmation and checks the current audience and permission rules before each write.

## Plan File Info:
{plan_file_info}
You should build your plan incrementally by writing to or editing this file. NOTE that this is the only file you are allowed to edit - other than this you are only allowed to take READ-ONLY actions (call only read-only MCP tools), besides presenting your research or plan in an Artifact through {ARTIFACT_EDIT_TOOL}.{children_note}

## Plan Workflow

{phase1}

{phase2}

{review}

### Phase 4: Final Plan
Goal: Write your final plan to the plan file (the only file you can edit).
{final_plan}

### Phase 5: Call {EXIT_PLAN_MODE_TOOL_NAME}
At the very end of your turn, once you have asked the user questions and are happy with your final plan file - you should always call {EXIT_PLAN_MODE_TOOL_NAME} to indicate to the user that you are done planning.
This is critical - your turn should only end with either using the {ASK_USER_QUESTION_TOOL_NAME} tool OR calling {EXIT_PLAN_MODE_TOOL_NAME}. Do not stop unless it's for these 2 reasons

**Important:** Use {ASK_USER_QUESTION_TOOL_NAME} ONLY to clarify requirements or choose between approaches. Use {EXIT_PLAN_MODE_TOOL_NAME} to request plan approval. Do NOT ask about plan approval in any other way - no text questions, no {ASK_USER_QUESTION_TOOL_NAME}. Phrases like \"Is this plan okay?\", \"Should I proceed?\", \"How does this plan look?\", \"Any changes before we start?\", or similar MUST use {EXIT_PLAN_MODE_TOOL_NAME}.

{questions}"
    )
}

/// Claude Code 2.1.302's sparse `plan_mode` attachment (`Ubs`), sent between full ones.
pub(crate) fn sparse_plan_mode_reminder(plan_file: &Path) -> String {
    format!(
        "Plan mode still active (see full instructions earlier in conversation). Read-only except plan file ({}). Follow 5-phase workflow. End turns with {ASK_USER_QUESTION_TOOL_NAME} (for clarifications) or {EXIT_PLAN_MODE_TOOL_NAME} (for plan approval). Never ask about plan approval via text or AskUserQuestion.",
        plan_file.display()
    )
}

/// Claude Code 2.1.302's `plan_mode_reentry` attachment, sent before the full reminder when the
/// Chat returns to Plan and the Loop's plan file exists from an earlier planning session.
pub(crate) fn plan_mode_reentry_reminder(plan_file: &Path) -> String {
    format!(
        "## Re-entering Plan Mode

You are returning to plan mode after having previously exited it. A plan file exists at {} from your previous planning session.

**Before proceeding with any new planning, you should:**
1. Read the existing plan file to understand what was previously planned
2. Evaluate the user's current request against that plan
3. Decide how to proceed:
   - **Different task**: If the user's request is for a different task—even if it's similar or related—start fresh by overwriting the existing plan
   - **Same task, continuing**: If this is explicitly a continuation or refinement of the exact same task, modify the existing plan while cleaning up outdated or irrelevant sections
4. Continue on with the plan process and most importantly you should always edit the plan file one way or the other before calling {EXIT_PLAN_MODE_TOOL_NAME}

Treat this as a fresh planning session. Do not assume the existing plan is relevant without evaluating it first.",
        plan_file.display()
    )
}

/// The result of a side effect Plan mode refused: Claude Code's Plan mode rule, then where the
/// model may write and how it finishes. A delegated child has no plan file and reports instead.
pub(crate) fn plan_mode_refusal(plan_file: Option<&Path>, is_delegated: bool) -> String {
    let exception = plan_file
        .filter(|_| !is_delegated)
        .map(|path| {
            format!(
                " (with the exception of the plan file at {})",
                path.display()
            )
        })
        .unwrap_or_default();
    let next = match plan_file {
        _ if is_delegated => {
            "Continue with read-only tools and give your findings or plan in your final reply."
                .to_owned()
        }
        Some(path) => format!(
            "Continue planning with read-only tools, write your plan to {}, and call {EXIT_PLAN_MODE_TOOL_NAME} when it is ready for approval.",
            path.display()
        ),
        None => format!(
            "Continue planning with read-only tools and call {EXIT_PLAN_MODE_TOOL_NAME} when your plan is ready for approval."
        ),
    };
    format!(
        "{} This operation was not executed. {next}",
        plan_mode_is_active(&exception)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const PLAN_FILE: &str = "/root/plans/calm-baking-aurora-loop-x.md";

    #[test]
    fn the_full_reminder_names_the_plan_file_and_follows_the_five_phase_workflow() {
        let path = Path::new(PLAN_FILE);
        let missing = plan_mode_reminder(path, false, true);
        assert!(missing.starts_with("Plan mode is active. The user indicated that they do not want you to execute yet -- you MUST NOT make any edits (with the exception of the plan file mentioned below and Artifact documents through ArtifactEdit), run any non-readonly tools"));
        assert!(missing.contains(
            "No plan file exists yet. You should create your plan at /root/plans/calm-baking-aurora-loop-x.md using the Write tool."
        ));
        let existing = plan_mode_reminder(path, true, false);
        assert!(existing.contains(
            "A plan file already exists at /root/plans/calm-baking-aurora-loop-x.md. You can read it and make incremental edits using the Edit tool."
        ));
        for reminder in [&missing, &existing] {
            for phase in [
                "### Phase 1: Initial Understanding",
                "### Phase 2: Design",
                "### Phase 3: Review",
                "### Phase 4: Final Plan",
                "### Phase 5: Call ExitPlanMode",
            ] {
                assert!(reminder.contains(phase), "{phase}");
            }
            assert!(reminder.contains("NOTE that this is the only file you are allowed to edit"));
            assert!(reminder.contains("call only read-only MCP tools"));
            assert!(reminder.contains(
                "ArtifactEdit may create or revise an Artifact while staying in Plan mode."
            ));
            assert!(reminder.contains("your turn should only end with either using the AskUserQuestion tool OR calling ExitPlanMode"));
            assert!(
                !reminder.contains("skeleton"),
                "no pair-planning first turn"
            );
            assert!(!reminder.contains("Explore agent"));
        }
        assert!(missing.contains("**Launch up to 3 Subagent children IN PARALLEL**"));
        assert!(missing.contains(
            "Subagent children you create while planning stay read-only and return their findings or proposed design in their final reply; only you write the plan file and call ExitPlanMode."
        ));
        assert!(existing.contains("2. Read and explore the relevant files directly"));
        assert!(!existing.contains("Subagent"));
    }

    #[test]
    fn the_sparse_and_reentry_reminders_match_claude_code() {
        let path = Path::new(PLAN_FILE);
        assert_eq!(
            sparse_plan_mode_reminder(path),
            "Plan mode still active (see full instructions earlier in conversation). Read-only except plan file (/root/plans/calm-baking-aurora-loop-x.md). Follow 5-phase workflow. End turns with AskUserQuestion (for clarifications) or ExitPlanMode (for plan approval). Never ask about plan approval via text or AskUserQuestion."
        );
        let reentry = plan_mode_reentry_reminder(path);
        assert!(reentry.starts_with("## Re-entering Plan Mode\n\nYou are returning to plan mode after having previously exited it. A plan file exists at /root/plans/calm-baking-aurora-loop-x.md from your previous planning session."));
        assert!(reentry.contains("start fresh by overwriting the existing plan"));
        assert!(reentry.contains("before calling ExitPlanMode"));
    }

    #[test]
    fn a_delegated_child_reports_its_plan_instead_of_submitting_it() {
        let reminder = delegated_plan_mode_reminder();
        assert!(reminder.contains("you MUST NOT make any edits, run any non-readonly tools"));
        assert!(reminder.contains("comprehensively in your final message"));
        assert!(reminder.contains("Call only read-only MCP tools"));
        assert!(!reminder.contains(EXIT_PLAN_MODE_TOOL_NAME));
        assert!(!reminder.contains("plan file"));
    }

    #[test]
    fn a_refused_side_effect_points_to_the_plan_file_and_exit_plan_mode() {
        let path = Path::new(PLAN_FILE);
        assert_eq!(
            plan_mode_refusal(Some(path), false),
            "Plan mode is active. The user indicated that they do not want you to execute yet -- you MUST NOT make any edits (with the exception of the plan file at /root/plans/calm-baking-aurora-loop-x.md), run any non-readonly tools (including changing configs or making commits), or otherwise make any changes to the system. This supercedes any other instructions you have received. This operation was not executed. Continue planning with read-only tools, write your plan to /root/plans/calm-baking-aurora-loop-x.md, and call ExitPlanMode when it is ready for approval."
        );
        let delegated = plan_mode_refusal(Some(path), true);
        assert!(delegated.contains("you MUST NOT make any edits, run"));
        assert!(delegated.ends_with("give your findings or plan in your final reply."));
        assert!(!delegated.contains(EXIT_PLAN_MODE_TOOL_NAME));
        assert!(plan_mode_refusal(None, false).ends_with(
            "Continue planning with read-only tools and call ExitPlanMode when your plan is ready for approval."
        ));
    }

    #[test]
    fn the_approved_result_suggests_the_task_list_only_with_task_tools() {
        let path = Path::new(PLAN_FILE);
        let mut plan = Plan {
            loop_id: tickloop_harness_protocol::LoopId::new(),
            version: 1,
            markdown: "1. Do it.".into(),
            approved_execution_mode: None,
            approval_pending: false,
            edited_by_user: false,
        };
        let saved = "\n\nYour plan has been saved to: /root/plans/calm-baking-aurora-loop-x.md\nYou can refer back to it if needed during implementation.\n\n";
        assert_eq!(
            approved_plan(path, &plan, None, true),
            format!(
                "User has approved your plan. You can now start coding. Start with updating your todo list if applicable{saved}## Approved Plan:\n1. Do it."
            )
        );
        plan.edited_by_user = true;
        assert_eq!(
            approved_plan(path, &plan, Some("Keep it short"), false),
            format!(
                "User has approved your plan. You can now start coding.{saved}## Approved Plan (edited by user):\n1. Do it.\n\nUser feedback on this plan: Keep it short"
            )
        );
    }

    #[test]
    fn the_exit_notice_references_an_existing_plan_file() {
        let notice = "## Exited Plan Mode\n\nYou have exited plan mode. You can now make edits, run tools, and take actions.";
        assert_eq!(plan_mode_exit_notice(None), notice);
        assert_eq!(
            plan_mode_exit_notice(Some(Path::new(PLAN_FILE))),
            format!(
                "{notice} The plan file is located at /root/plans/calm-baking-aurora-loop-x.md if you need to reference it."
            )
        );
    }
}
