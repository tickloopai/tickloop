//! Plan mode and todo list texts the model reads, adapted from Claude Code's `EnterPlanModeTool`,
//! `ExitPlanModeTool` and `TodoWriteTool` prompts and results and the plan mode reminders of
//! `utils/messages.ts`. The planning reminder uses direct Read, Glob and Grep exploration.

use std::path::Path;

use tickloop_agent_tools::{
    ASK_USER_QUESTION_TOOL_NAME, EDIT_TOOL_NAME, ENTER_PLAN_MODE_TOOL_NAME,
    EXIT_PLAN_MODE_TOOL_NAME, GLOB_TOOL_NAME, GREP_TOOL_NAME, READ_TOOL_NAME, WRITE_TOOL_NAME,
};

pub(super) const NOT_IN_PLAN_MODE: &str = "You are not in plan mode. This tool is only for exiting plan mode after writing a plan. If your plan was already approved, continue with implementation.";

pub(super) const TODO_WRITE_RESULT: &str = "Todos have been modified successfully. Ensure that you continue to use the todo list to track your progress. Please proceed with the current tasks if applicable";

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

Remember: DO NOT write or edit any files except the plan file. This is a read-only exploration and planning phase."
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
        "User has approved your plan. You can now start coding. Start with updating your todo list if applicable

Your plan has been saved to: {}
You can refer back to it if needed during implementation.

## Approved Plan:
{markdown}",
        path.display()
    )
}

/// The plan mode section of every conversation request while the Chat is in Plan mode.
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

Plan mode is active. The user indicated that they do not want you to execute yet -- you MUST NOT make any edits (with the exception of the plan file mentioned below), run any non-readonly tools (including changing configs or making commits), or otherwise make any changes to the system. This supercedes any other instructions you have received.

## Plan File Info:
{plan_file_info}
You should build your plan incrementally by writing to or editing this file. NOTE that this is the only file you are allowed to edit - other than this you are only allowed to take READ-ONLY actions.

## Iterative Planning Workflow

You are pair-planning with the user. Explore the code to build context, ask the user questions when you hit decisions you can't make alone, and write your findings into the plan file as you go. The plan file (above) is the ONLY file you may edit — it starts as a rough skeleton and gradually becomes the final plan.

### The Loop

Repeat this cycle until the plan is complete:

1. **Explore** — Use {READ_TOOL_NAME}, {GLOB_TOOL_NAME}, {GREP_TOOL_NAME} to read code. Look for existing functions, utilities, and patterns to reuse.
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

/// Claude Code's `TodoWrite` prompt; the todo list belongs to the current goal rather than a session.
pub(super) fn todo_write_description() -> String {
    format!(
        r#"Use this tool to create and manage a structured task list for your current goal. This helps you track progress, organize complex tasks, and demonstrate thoroughness to the user.
It also helps the user understand the progress of the task and overall progress of their requests.

## When to Use This Tool
Use this tool proactively in these scenarios:

1. Complex multi-step tasks - When a task requires 3 or more distinct steps or actions
2. Non-trivial and complex tasks - Tasks that require careful planning or multiple operations
3. User explicitly requests todo list - When the user directly asks you to use the todo list
4. User provides multiple tasks - When users provide a list of things to be done (numbered or comma-separated)
5. After receiving new instructions - Immediately capture user requirements as todos
6. When you start working on a task - Mark it as in_progress BEFORE beginning work. Ideally you should only have one todo as in_progress at a time
7. After completing a task - Mark it as completed and add any new follow-up tasks discovered during implementation

## When NOT to Use This Tool

Skip using this tool when:
1. There is only a single, straightforward task
2. The task is trivial and tracking it provides no organizational benefit
3. The task can be completed in less than 3 trivial steps
4. The task is purely conversational or informational

NOTE that you should not use this tool if there is only one trivial task to do. In this case you are better off just doing the task directly.

## Examples of When to Use the Todo List

<example>
User: I want to add a dark mode toggle to the application settings. Make sure you run the tests and build when you're done!
Assistant: *Creates todo list with the following items:*
1. Creating dark mode toggle component in Settings page
2. Adding dark mode state management (context/store)
3. Implementing CSS-in-JS styles for dark theme
4. Updating existing components to support theme switching
5. Running tests and build process, addressing any failures or errors that occur
*Begins working on the first task*

<reasoning>
The assistant used the todo list because:
1. Adding dark mode is a multi-step feature requiring UI, state management, and styling changes
2. The user explicitly requested tests and build be run afterward
3. The assistant inferred that tests and build need to pass by adding "Ensure tests and build succeed" as the final task
</reasoning>
</example>

<example>
User: Help me rename the function getCwd to getCurrentWorkingDirectory across my project
Assistant: *Uses grep or search tools to locate all instances of getCwd in the codebase*
I've found 15 instances of 'getCwd' across 8 different files.
*Creates todo list with specific items for each file that needs updating*

<reasoning>
The assistant used the todo list because:
1. First, the assistant searched to understand the scope of the task
2. Upon finding multiple occurrences across different files, it determined this was a complex task with multiple steps
3. The todo list helps ensure every instance is tracked and updated systematically
4. This approach prevents missing any occurrences and maintains code consistency
</reasoning>
</example>


<example>
User: I need to implement these features for my e-commerce site: user registration, product catalog, shopping cart, and checkout flow.
Assistant: *Creates a todo list breaking down each feature into specific tasks based on the project architecture*
Let's start with user registration. This will involve creating database models, API endpoints, and frontend forms that integrate with your existing authentication system.

<reasoning>
The assistant used the todo list because:
1. The user provided multiple complex features to implement in a comma separated list
2. The todo list helps organize these large features into manageable tasks
3. This approach allows for tracking progress across the entire implementation
</reasoning>
</example>

<example>
User: Can you help optimize my React application? It's rendering slowly and has performance issues.
Assistant: *Reviews component structure, render patterns, state management, and data fetching*
After analyzing your codebase, I've identified several performance issues.
*Creates todo list with items like: 1) Implementing memoization for expensive calculations in ProductList, 2) Adding virtualization for long lists in Dashboard, 3) Optimizing image loading in Gallery component, 4) Fixing state update loops in ShoppingCart, 5) Reviewing bundle size and implementing code splitting*
Let's start by implementing memoization for the expensive calculations in your ProductList component.

<reasoning>
The assistant used the todo list because:
1. First, the assistant examined the codebase to identify specific performance issues
2. Based on this analysis, it identified multiple optimization opportunities
3. Performance optimization is a non-trivial task requiring multiple steps
4. The todo list helps methodically track improvements across different components
5. This systematic approach ensures all performance bottlenecks are addressed
</reasoning>
</example>

## Examples of When NOT to Use the Todo List

<example>
User: How do I print 'Hello World' in Python?
Assistant: In Python, you can print "Hello World" with this simple code:

python
print("Hello World")

This will output the text "Hello World" to the console when executed.

<reasoning>
The assistant did not use the todo list because this is a single, trivial task that can be completed in one step. There's no need to track multiple tasks or steps for such a straightforward request.
</reasoning>
</example>

<example>
User: What does the git status command do?
Assistant: The git status command shows the current state of your working directory and staging area. It displays which changes have been staged, which haven't, and which files aren't being tracked by Git. This is a fundamental command for understanding the current state of your repository before making commits.

<reasoning>
The assistant did not use the todo list because this is an informational request with no actual coding task to complete. The user is simply asking for an explanation, not for the assistant to perform multiple steps or tasks.
</reasoning>
</example>

<example>
User: Can you add a comment to the calculateTotal function to explain what it does?
Assistant: Sure, let me add a comment to the calculateTotal function to explain what it does.
* Uses the {EDIT_TOOL_NAME} tool to add a comment to the calculateTotal function *

<reasoning>
The assistant did not use the todo list because this is a single, straightforward task confined to one location in the code. Adding a comment doesn't require tracking multiple steps or systematic organization.
</reasoning>
</example>

<example>
User: Run npm install for me and tell me what happens.
Assistant: I'll run the npm install command for you.

*Executes: npm install*

The command completed successfully. Here's the output:
[Output of npm install command]

All dependencies have been installed according to your package.json file.

<reasoning>
The assistant did not use the todo list because this is a single command execution with immediate results. There are no multiple steps to track or organize, making the todo list unnecessary for this straightforward task.
</reasoning>
</example>

## Task States and Management

1. **Task States**: Use these states to track progress:
   - pending: Task not yet started
   - in_progress: Currently working on (limit to ONE task at a time)
   - completed: Task finished successfully

   **IMPORTANT**: Task descriptions must have two forms:
   - content: The imperative form describing what needs to be done (e.g., "Run tests", "Build the project")
   - activeForm: The present continuous form shown during execution (e.g., "Running tests", "Building the project")

2. **Task Management**:
   - Update task status in real-time as you work
   - Mark tasks complete IMMEDIATELY after finishing (don't batch completions)
   - Exactly ONE task must be in_progress at any time (not less, not more)
   - Complete current tasks before starting new ones
   - Remove tasks that are no longer relevant from the list entirely

3. **Task Completion Requirements**:
   - ONLY mark a task as completed when you have FULLY accomplished it
   - If you encounter errors, blockers, or cannot finish, keep the task as in_progress
   - When blocked, create a new task describing what needs to be resolved
   - Never mark a task as completed if:
     - Tests are failing
     - Implementation is partial
     - You encountered unresolved errors
     - You couldn't find necessary files or dependencies

4. **Task Breakdown**:
   - Create specific, actionable items
   - Break complex tasks into smaller, manageable steps
   - Use clear, descriptive task names
   - Always provide both forms:
     - content: "Fix authentication bug"
     - activeForm: "Fixing authentication bug"

When in doubt, use this tool. Being proactive with task management demonstrates attentiveness and ensures you complete all requirements successfully.
"#
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
            assert!(reminder.contains("NOTE that this is the only file you are allowed to edit"));
            assert!(
                reminder.contains("- Calling ExitPlanMode when the plan is ready for approval")
            );
        }
    }
}
