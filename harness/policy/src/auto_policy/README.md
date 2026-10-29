# Frozen Default Policy Integration

Source: `auto-mode defaults` from Claude Code 2.1.296, binary SHA-256
`c9b5341637becbd423ddffc5b254afb645682a3868cb708bbc6cc0e7bb419937`.

The full text and specific authorization thresholds of applicable rules are kept; each stage still produces only one overall model question.
The mapping is verified by config tests and a Jev test double; test-double results are not semantic acceptance by the real Jev.

## Product Mapping

- Session-Created Job Cleanup
- Claude Code Scheduling
- Scheduled-Task Fires
- Multi-Agent Coordination
- CLAUDE.md Content
- Unrequested Artifact Publish
- Live-Shared Artifact Sensitive Delta
- Containment Escape
- Self-Modification
- Tmux Self Drive
- Instruction Poisoning
- Auto-Mode Bypass
- Session Transcript Tampering

The entries above replace only the actual product interfaces, sources or scope; Artifact account roles and collaboration permissions are still enforced by the original host.
Tick cleanup must match a trusted creation credential from the same Loop and instance. Automatic triggers and Agent messages do not create new human authorization.

## Scheduled Tasks and Goal

The native **Claude Code Scheduling** rule in 2.1.296 is a Soft allow covering CronCreate, CronDelete,
CronList and RemoteTrigger; its text explicitly allows prompt delivery within the current session, and persistent tasks written to
`.claude/scheduled_tasks.json`. Tickloop keeps the original text and then narrows it to the actual product mapping:
only schedules for the current Chat are allowed, and RemoteTrigger has no corresponding entry point; self-paced ScheduleWakeup also falls under this mapping.

Verification entry points: in Claude Code, `autoModeDefaultsHandler` in `src/cli/handlers/autoMode.ts` calls
`getDefaultExternalAutoModeRules` in `src/utils/permissions/yoloClassifier.ts`, which extracts the rules from the external policy template.
The local read-only source lacks that template, so the rule text is exported from the official 2.1.296 binary matching the SHA-256 above.
`src/tools/ScheduleCronTool/CronListTool.ts` explicitly sets `isReadOnly() = true`; CronCreate and
CronDelete have no such declaration, so they inherit the non-read-only default from `src/Tool.ts` and are decided by the classifier policy.

| Tickloop tool | Permission and Auto handling | Rule basis and authorization boundary |
| --- | --- | --- |
| CronList | ReadOnly; no review | Read-Only Operations; Claude Code's read-only setting for CronList |
| GetGoal | ReadOnly; no review | Reads only the current Chat's Goal; integrated following Codex get_goal, not policy-classified |
| CronCreate | Classified; Scheduling Soft allow | Only schedules in the current Chat, within the user's task scope and time limit; does not pre-approve future actual operations |
| ScheduleWakeup | Classified; Scheduling Soft allow | Rescheduling of an already-authorized self-paced `/loop` in the current Chat; stops this loop when the task is achieved or the user asks to cancel, and does not manage other tasks |
| CronUpdate | Classified; conditional Scheduling Soft allow | Modifies one task of the current Chat in place (schedule, recurrence, instruction or title); Agent tasks for which this Loop holds an exact successful creation credential may be modified; tasks the user explicitly asks to modify may be modified; after modification the task stays within the user-authorized task scope and time limit; user tasks without modification authorization are still blocked by Interfere With Workloads |
| CronDelete | Classified; conditional Scheduling Soft allow | Agent tasks for which this Loop holds an exact successful creation credential may be cleaned up; tasks the user explicitly asks to cancel may be deleted; user tasks without cancellation authorization are still blocked by Interfere With Workloads |
| CreateGoal | Automatic; Auto executes directly, no review | Integrated following Codex create_goal's direct-execution approach; constraints such as the user explicitly requesting a Goal and the token budget remain in the tool instructions |
| UpdateGoal | ReadOnly; no review | Only reports completion or blockage and changes no state outside the Goal; integrated following Codex update_goal, and completion requests still go through an independent completion check |

The `scheduled_task` host facts for CronUpdate and CronDelete describe only the exact target id, creator and verifiable
creation_call_id within that Chat, plus size-limited schedule/prompt details, so that modification or cancellation requests can resolve the target.
These details are data, not new user authorization. A creation credential requires a completed CronCreate in the same Loop whose successful result,
as recorded by the host, returns that target id; a matching successful tool history is also required. Only a task whose creator is Agent can use this cleanup exception.
User tasks created in the turn of a user `/loop` are not miscounted as Agent-created tasks either. Finding an id, model narration,
a task being newer, or records from other Loops do not prove ownership; without a credential, only a real user's specific cancellation authorization can be relied on.

**Unauthorized Persistence** and **Account & Standing-Rule Changes** are still kept in full.
Native Chat scheduled tasks may span turns, continue after the view closes, and are saved with the Chat; Scheduling
clears these two Soft deny rules only for these authorized arrangements of this product.
OS cron, systemd, shell profiles, access credentials, automations/webhooks of connected accounts and the like are not covered by the exception.
CronCreate must not be misjudged as unauthorized persistence merely because it defers execution, nor can Scheduling be used to authorize future sensitive operations.
**Scheduled-Task Fires** still states: automatic delivery does not create new human authorization and keeps the scope and time limit of the original authorization.
Claude Code has no corresponding Goal tools. The three Goal tools are integrated following Codex's direct-execution approach
and do not use the Auto policy mapping; there is no Chat Goal Lifecycle Soft allow. GetGoal and UpdateGoal only read or report
the Goal and are marked `HostToolPermission::ReadOnly` (also available under Plan); CreateGoal is marked
`HostToolPermission::Automatic`: allowed directly under Auto, not executed under Plan, and explicit Ask/Deny rules still take precedence.
Constraints such as the user explicitly requesting a Goal, the token budget and the number of continuations when blocked are still enforced by the tool instructions;
CreateGoal's existing-Goal check, UpdateGoal's independent completion check and the Goal state machine are unaffected by the permission category.

Subagents are not given CronCreate, CronList, CronUpdate, CronDelete, ScheduleWakeup, GetGoal,
CreateGoal or UpdateGoal; no separate subagent mapping is needed, and delegation cannot expand the root Agent's permissions.

For a stateless host's durable Chat, host rules directly admit tools such as CronCreate, CronUpdate, CronDelete and ScheduleWakeup
without Auto review; see `durable_chat_host_rules` in `harness/runtime/src/harness.rs`.

## Tick tools without Herdr Pane views

Ticks are no longer shown in Herdr Panes. `ReadWorkspace` (previously ReadOnly) is no longer offered, and
TickManage takes only `open`, `send_input`, `close` and `reopen` as host forms (with the Loop's own
`subscribe` and `unsubscribe`); `open` takes `command`, `cwd` and `protocol`, no longer `placement` or
`workspace_id`, and `move` is gone. TickDescribe and TickRead stay ReadOnly; the remaining TickManage
forms stay Reviewable, so Auto still reviews an ordinary open under the existing rules, and closing an
instance this Loop created still relies on Session-Created Job Cleanup. A call carrying a removed form
or field, such as one repeated from an older conversation, fails input validation before permission,
review or dispatch; nothing is widened. Saved `ModifyTick` rules still map to the remaining forms.
No rules are added, removed or remapped: **15 Soft allow, 65 Soft deny, 2 Hard deny, 82 total**,
eight inapplicable native entries, and policy and built-in group version **10** are unchanged.
`harness/runtime/tests/all/environment_approval.rs` covers the routing with the reference Execution
Environment and a reviewer double.

## Sandbox Network Callback

This native entry is remapped to Tickhost's deterministic HTTP/HTTPS CONNECT proxy.
The user's `sandbox.network.allowedDomains` is frozen with each prepared Shell and its
single-use spawn authorization. Only matching destinations are reachable through that
Shell's authenticated route through the Tickhost's lifetime-bound loopback proxy; direct network access remains blocked. A denied domain is
reported in the Shell output. The proxy neither asks a model nor grants a new domain;
only the user can change the allowlist in their settings. The proxy checks a random per-Tickhost token and a private per-Shell routing identity; a Shell's route is revoked at exit without releasing the listener. Detached descendants surviving a Tickhost restart retain the old OS port permission, as documented in the README sandbox limitation. There is no model-callable
SandboxNetworkAccess tool, and this mapping adds no policy rule. Existing permission
rules and the separate sandboxed-Bash admission path retain their own conditions.

## Native Entries Not Currently Applicable

- Memory Directory: No managed memory directory is provided by Tickloop.
- Browser Trusted Navigation: The authenticated Chrome-MCP browser surface is not exposed.
- Command Network Lists: Bash does not grant network access through allowed_domains.
- Browser Navigate Exfil: The authenticated Chrome-MCP browser surface is not exposed.
- Browser Input Exfil: The authenticated Chrome-MCP browser surface is not exposed.
- Browser JS Exfil: The authenticated Chrome-MCP browser surface is not exposed.
- Browser File Upload Exfil: The authenticated Chrome-MCP browser surface is not exposed.
- Browser Shortcut Execution: The authenticated Chrome-MCP browser surface is not exposed.

Not applicable means only that the specific entry point does not exist; it does not exempt real side effects of the same kind. Data exfiltration,
persistence or out-of-scope actions carried out through Bash or other exposed tools are still judged by the corresponding general rules. When a corresponding product entry point is added, it must be remapped.

There are currently **15 Soft allow, 65 Soft deny, 2 Hard deny, 82 rules in total**; the policy and built-in group versions are both 10.
One of the two Hard deny rules is Tickloop Agent Daemon Shutdown. There are currently eight native entry points that are not applicable.
Custom rules and group replacements stay independent; users who replace the built-in Soft allow are not forced to have the Scheduling exception added back.

## Third-party MCP tools

`mcp__<server>__<tool>` uses `HostToolPermission::ThirdParty`. Following Claude Code (`permissions.ts`: MCP tools are not on the safe-tool allowlist, so Auto sends them to `classifyYoloAction`), without a user rule Ask requires human confirmation, Auto sends the call to Auto review, and Full Access allows it. Plan decides the call as the mode it was entered from (Claude Code's `prePlanMode`; see Plan with Auto below): Auto review after Auto, Allow after Full Access, and human confirmation after Ask, when that mode is unknown (a Chat that starts in Plan, or after a load, rewind or Branch switch) and, with `useAutoModeDuringPlan` off, after Auto too. The Plan reminder tells the model to call only read-only MCP tools. Other tools with side effects keep their existing Plan handling. Server annotations never grant permission or skip review. User service names containing `__` are rejected by settings resolution and `mcp add`; plugin names and normalized name components collapse underscore runs, with duplicate resolved plugin names skipped and warned. Hashing preserves the `mcp__<server>__<tool>` separators. Consequently a bare service rule cannot also be a tool name; rules need no extra scope field. Explicit Ask and Deny still take precedence over Allow and over Auto review: an explicit Ask is never approved by the reviewer. An Allow rule admits the call in Plan as well, as in Claude Code. The user can save `mcp__<server>` (equivalently `mcp__<server>__*`) or `mcp__<server>__<tool>` in the current directory partition of their own settings. These MCP Allow rules remain effective in Auto; a broad `*` Allow still follows the existing Auto filtering. Service rules use the prepared call’s resolved service, including when the model name was hashed. Plugin approval fields and server annotations cannot create grants. Children share the connection but intersect their own rules with the frozen delegation ceiling and current parent restrictions.

| Tool | Permission and Auto handling | Authorization boundary |
| --- | --- | --- |
| Third-party `mcp__<server>__<tool>` | ThirdParty; Ask: human confirmation; Auto: classifier review without a user rule; Plan: as the mode it was entered from, so classifier review only after Auto with `useAutoModeDuringPlan` on, Allow after Full Access, otherwise human confirmation | Review judges the single call against the general rules; a rejection returns to the model and escalates to a person at the existing denial limits. A user-saved tool/server rule in this directory skips review; explicit Ask is single-use only |
| ListMcpResourcesTool / ReadMcpResourceTool | ReadOnly; no review | Prepared service authority and delegated resource scope still apply |

Review uses the existing frozen rules: the MCP tool name and the model-written input are the proposed action, while server instructions, descriptions, annotations and results stay untrusted data and are redacted of the server's sensitive values before they reach any model. No rule is added or removed: the 82-rule policy and eight inapplicable native entry points above are unchanged. Review is not waived by an Unrestricted host (`--dangerously-skip-permissions`): there a third-party call is still reviewed in Auto and still asks in Ask, and an escalation reaches a person; a host that cannot ask denies what the reviewer does not allow. Resource discovery and reading use ReadOnly admission with the same frozen server authority.

The browser-specific entries above remain inapplicable: connecting an arbitrary MCP browser does not implement the trusted native browser admission paths. Its tools follow ThirdParty admission, so in Auto the general rules (exfiltration, out-of-scope actions and the rest) judge them like any other action. Automated coverage is in `harness/runtime/tests/all/third_party_approval.rs`, the Fold and MCP server tests and the Console MCP confirmation tests: an admitted call runs without a prompt, a rejected one is not dispatched and the third consecutive rejection asks a person, and batch calls carry the whole batch. These use a reviewer test double; they prove deterministic routing, persistence and denial boundaries, not semantic acceptance by a real reviewer.

Permission history uses the same prepared service identity for matching as admission, and records the original service rule ID and matcher for Allow, Ask and Deny, including tools whose displayed names were hashed.

## Plan with Auto

Claude Code 2.1.302 keeps Auto active in Plan when the `useAutoModeDuringPlan` setting is on, its default (`prepareContextForPlanMode`, `isAutoModeActive`), whichever mode Plan was entered from. Tickloop deliberately differs: entering Plan never hands a person's approval to the reviewer, so Plan uses Auto's admission only when it was entered from Auto, whether the user switched to Plan or the model called EnterPlanMode. Tickloop reads the setting from the user's `settings.json` when a Chat is created, resumed or branched; it is filled into the permission policy like the host's rules, never recorded, and children take their parent's. The mode Plan was entered from (`PermissionPolicy::mode_before_plan`) is likewise never recorded: it is kept in this process, by children and their frozen ceilings, and across a planned upgrade, and a Chat that starts in Plan, or a load, rewind or Branch switch into Plan, leaves it unknown, which decides as Ask. `PermissionPolicy::admission_mode` is the single place that turns these into the mode Plan decides as: `ExecutionMode::leaving_plan`, the mode plan approval and ExitPlanMode continue in, so Plan never admits more than the mode it will hand back, except that after Auto with the setting off it asks. Plan first refuses its side effects as before (`Write`, `Edit`, mutating `Bash`, Automatic, Reviewable and Confirmation host tools; Artifact writes still ask). Entered from Auto with the setting on, everything it would otherwise ask about then follows Auto: broad user and project Allow rules are set aside, the Accept Edits fast path and the safe-tool allowlist admit, and the rest goes to the classifier, so `WebFetch` and reads outside the workspace are admitted or reviewed instead of asked, and Classified host tools are reviewed. Entered from Full Access, whatever the setting, Plan allows everything it does not refuse, including `WebFetch`, reads outside the workspace, third-party MCP calls and Classified host tools, without asking or review. This matches Claude Code, whose Plan allows when bypass permissions are available in the session (`isBypassPermissionsModeAvailable`, which entering Plan from bypass implies; `oN`, `Nnt` and the core permission check in 2.1.302), after explicit Deny and Ask rules, bypass-immune safety checks and tools that require user interaction; Claude Code's bypass also overrides its own Plan refusal of non-read-only tools, while Tickloop keeps refusing Plan's side effects first, so Tickloop stays narrower there. Artifact writes still ask in Plan. Otherwise Plan asks for what Ask would ask about and admits Classified host tools as Ask does, and the reviewer never runs. Third-party MCP calls follow the same rule. No rule text changes: the 82-rule policy and eight inapplicable native entries remain unchanged.

## Plan file

Following Claude Code 2.1.302 (`checkWritePermissionForTool` admits the session's plan files before its other checks), the current Loop's plan file is readable and writable without asking in every execution mode, not only in Plan: `Read`, `Write` and `Edit` of exactly that file are allowed before the mode's admission, the Accept Edits fast path and Auto review, so Auto never sends them to the classifier. Explicit Ask and Deny rules still win. In Plan it remains the only writable file, and other side effects are refused with Claude Code's Plan mode rule, the plan file's path and a pointer to ExitPlanMode. Delegated children have no plan file. ExitPlanMode stays outside the policy: with a plan it opens the plan approval, and without one (a missing or blank plan file) it asks the user Claude Code's "Exit plan mode?" yes/no question, an explicit Ask that only a person answers; yes restores the mode Plan was entered from (Ask when unknown). No rule text changes: the 82-rule policy and eight inapplicable native entries remain unchanged.

## Fold and nested calls

| Tool | Permission and Auto handling | Authorization boundary |
| --- | --- | --- |
| SearchTools / DescribeTools | ReadOnly; no review | Discovery respects exposure and the delegated tool ceiling; annotations are reference data only |
| ExecuteTools | No wrapper approval | Each independent child call is prepared, approved and dispatched through the ordinary tool pipeline, with the same target permission and policy revalidation |
| EnterPlanMode | Deferred behind tool search, like Claude Code 2.1.302's `shouldDefer`; callable directly, or inside ExecuteTools only as the batch's sole call (its calls run concurrently). No approval and no review in any mode, as in Claude Code, which gives it no permission check | Only an explicit Ask rule asks the user (the Enter plan mode dialog) and a Deny rule refuses it; it only switches the Chat to Plan, which narrows what later calls may do. Delegated children never get it |

Nested calls record their parent call and their individual permission decisions. Human confirmation pauses only the waiting call. Auto receives the entire model-authored batch and observed earlier results as assistant evidence, not user authorization, under the existing history trimming rules. An effective Auto rejection stops calls not yet dispatched, including pending confirmations; dispatched calls settle normally. Rejections count toward the existing denial limits. Fold does not add policy rules or native admission paths: the 82-rule policy and eight inapplicable entries remain unchanged.

## Linked worktrees of the Folder's repository

Claude Code 2.1.302 has a linked-worktree Accept Edits fast path behind a flag that is off by default, and it applies only when the session starts in the main checkout. Tickloop applies it in Auto whenever the Folder belongs to a git repository: the other linked worktrees registered in that repository count as workspace directories when Auto checks its Accept Edits fast path, so `Write`, `Edit` and Accept-Edits-admissible `Bash` into a sibling worktree run without review. Each worktree must link back to the same common git directory (`harness/base/src/git_worktree.rs`); the main checkout, worktrees enclosing the Folder or the git directory, unverified entries and more than 32 worktrees are left out. Sensitive paths, explicit Ask and Deny rules and the other fast-path conditions still apply, Accept Edits and Plan are unchanged. Delegated children still cannot `Write` or `Edit` outside their frozen workspace; their `Bash` gets the same Auto decision as the parent's.

Calls that still reach the reviewer, such as `cd <worktree> && …` compounds, carry `same_repository_worktrees` in the approval context (version `trusted-approval-v11`) so the reviewer can tell those checkouts from other repositories. No rule text changes: the 82-rule policy and eight inapplicable native entries remain unchanged.

## Commands outside the sandbox

Bash's per-call escape request uses ordinary command approval; it does not add a callback tool or grant a proxy domain.
When the user enables the local sandbox, `dangerouslyDisableSandbox: true` or a matching
`sandbox.excludedCommands` entry prepares that invocation with unrestricted writes and networking,
and with an execution-environment requirement for ordinary approval. The immutable preparation is
used for approval and execution; neither a model argument nor an exclusion is an approval grant.
Auto sends such calls to its reviewer instead of the read-only or Accept Edits fast paths; Ask
requires human approval. Explicit Permission Rules retain their precedence, Full Access retains its
normal behavior, and Plan retains its existing read-only handling. An Unrestricted prompt host does
not waive a required reviewer or human decision. Such calls never take the sandboxed Bash fast path
below.

The ordinary frozen policy judges the command's actual effects and the user's authorization.
Routine authorized builds are not automatically a Containment Escape merely because they request
this per-call route. Unauthorized reach, exfiltration and standing permission changes remain subject
to Containment Escape, the exfiltration rules and Self-Modification. Permission Rule Ask/Deny and
non-classifier-approvable safety checks cannot be cleared by the reviewer.

`sandbox.allowUnsandboxedCommands` defaults to true. When false, the model's request is ignored; explicitly excluded commands still run outside the
sandbox with ordinary approval. Other commands retain their sandbox. When the sandbox is disabled, the
request has no effect. These controls are read only from the user's settings by Code, never from
repository content or Harness Settings Store configuration.

No rules are added or removed: **15 Soft allow, 65 Soft deny, 2 Hard deny, 82 total**, with eight
inapplicable native entries. Policy and built-in group version **10** covers this mapping and the
Sandbox Network Callback remapping above.
Portable Agent tests in `harness/runtime/tests/all/environment_approval.rs` use the reference
Execution Environment, ScriptedModel and a reviewer double. Local integration tests in
`code/local-environment/tests/all/sandbox.rs` cover the settings and actual macOS execution.
Both verify admission and rejection, including normally safe commands. This is routing evidence,
not semantic acceptance by a real review model. The conditional escape hint is appended by Code
using frozen preparation state; the Tickhost policy wire shape remains compatible with 10.1.

## PreToolUse hooks

Claude Code–compatible `command` hooks are not model tools, so this mapping adds no rule. A call's `PreToolUse` hooks run before it is prepared, and their merged answer (deny over ask over allow) changes only how the call reaches the steps below; `harness/runtime/src/hooks/` holds the engine and `harness/runtime/tests/all/tool_hooks.rs` the routing tests with the reference Execution Environment and a reviewer double.

- **deny** (a `permissionDecision: "deny"`, `decision: "block"` or exit status 2, from any source) refuses the call unprepared: no Permission Rule, review or prompt is consulted, and the model reads the hook's reason.
- **ask** (from any source) sends the call to a person in every mode, Auto included, without the reviewer; the request carries the approval source `hook`, and like an explicit Ask the answer covers that one call and saves no rule. An explicit Deny rule or Plan's side-effect refusal still refuses first. An Unrestricted host still asks; a host that cannot ask refuses.
- **allow** counts only from a hook in the user's own `settings.json`. It admits the call after the deterministic checks and before the reviewer, as a user Allow rule does and as Claude Code does: explicit Ask and Deny rules, sensitive-path safety checks, Plan's side-effect refusal, approval the Execution Environment requires, a host tool's required confirmation, the delegation ceiling and final admission all still apply. Classified host tools whose Allow rules do not skip review (Subagent, the scheduled task tools) are still reviewed, as they are under a user Allow rule. The reviewer and therefore the frozen rules, Hard deny included, are not consulted for that call. Its approval record is `Direct` with the optional reason `hook`. The allow covers only input the user's hooks saw or wrote: when a plugin or repository hook's `updatedInput` is the final input, the allow is dropped and the call is decided as if no hook had allowed it. A plugin's or the repository's own allow is recorded as ignored and the call is decided as if the hook had not answered, so such hooks can never widen what runs.
- **if** narrows a hook to the calls a Permission Rule such as `Bash(git push*)` may match, judged as Deny and Ask rules judge calls and failing closed so guard hooks never miss a call: a compound command matches when any of its commands does, with redirections set aside; a command whose commands cannot be told apart, an `if` that is not a rule, and content no Permission Rule can match (such as file path patterns) all let the hook run. A tool call's rewrite cannot change a Tick call's `action`, which chose its route.
- **updatedInput** replaces the call's input before its single preparation; approval, review and execution all see the final input, the conversation keeps the input as the model wrote it, and the hook run record keeps the rewrite.

Unconfirmed or modified plugin and repository hooks never run, so they cannot affect any decision. Hooks that do not answer leave the ordinary routing unchanged. No rules are added, removed or remapped: **15 Soft allow, 65 Soft deny, 2 Hard deny, 82 total**, eight inapplicable native entries, and policy and built-in group version **10** are unchanged.

## Sandboxed Bash

Following Claude Code's `autoAllowBashIfSandboxed` (`checkSandboxAutoAllow` in `bashPermissions.ts`),
a Shell call (foreground or background `Bash`, including a delegated child's under its frozen parent
permissions) is allowed directly in Accept Edits and Auto when its preparation reports writes confined to the
Folder and additional directories and networking that is not unrestricted (an allowlist or
disconnected), and the execution environment's owner allows it. Locally that is the user's
`sandbox.autoAllowBashIfSandboxed`, default true, read by Code only from the user's settings. The
call is neither asked about nor sent to the reviewer, so none of the frozen rules is consulted for
it; the sandbox, not the classifier, bounds what it can write and reach. Its approval record is
`Direct` with the optional reason `sandbox`.

The fast path reads only the frozen preparation, after the deterministic checks: Plan's side-effect
refusal, explicit Deny and Ask rules (whole-tool and content rules, unlike Claude Code, which lets a
sandboxed command pass a whole-tool Ask) and safety checks. As in Claude Code it comes before
explicit Allow rules, so such a call is recorded as sandboxed even when an Allow rule also covers
it. A Shell whose environment requires approval never qualifies. A call outside the sandbox (`dangerouslyDisableSandbox`, `excludedCommands`, or a
fallback without isolation) reports unrestricted isolation and keeps ordinary approval. Plan and
Full Access are unchanged. A whole-environment isolation report, as a future virtual machine would
give, does not qualify. Sensitive paths stay unwritable inside the sandbox itself.

The local policy derives its writable roots and relative filesystem settings from the trusted Chat
Folder, never from Bash `cwd`. A cwd outside the Folder and additional directories requires ordinary
approval, including Auto review, while retaining the same sandbox.
Temporary roots and configured `allowWrite` paths do not by themselves qualify a cwd for fast admission.

No rules are added, removed or remapped: **15 Soft allow, 65 Soft deny, 2 Hard deny, 82 total**,
eight inapplicable native entries, and policy and built-in group version **10** are unchanged.
`harness/runtime/tests/all/sandbox_auto_approval.rs` covers the routing with the reference
Execution Environment and a reviewer double; `code/local-environment/tests/all/sandbox.rs` covers
the setting and actual macOS execution.

## Plugin Commands loaded by Skill

Skill remains ReadOnly guidance loading. A plugin Command's shell blocks become separate Bash
calls after argument expansion, each using the existing prepare, admission and execution path.
The Skill call's admission and its `allowed-tools` metadata grant no permission to these calls.
Auto uses the same frozen Bash rules, explicit Ask/Deny rules and sandbox admission as an ordinary
Bash call; Plan and the delegation ceiling still apply. Discovery and completion execute nothing.
User slash invocations with Shell preparation wait for their own turn; those Shell blocks cannot
execute inside another turn. Ordinary Skill guidance keeps its existing tool-boundary merging. No default rule or count changes.
