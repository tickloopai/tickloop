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
without Auto review; see `durable_chat_host_rules` in `agent/runtime/src/harness.rs`.

## Native Entries Not Currently Applicable

- Memory Directory: No managed memory directory is provided by Tickloop.
- Browser Trusted Navigation: The authenticated Chrome-MCP browser surface is not exposed.
- Sandbox Network Callback: There is no SandboxNetworkAccess callback in this runtime.
- Command Network Lists: Bash does not grant network access through allowed_domains.
- Browser Navigate Exfil: The authenticated Chrome-MCP browser surface is not exposed.
- Browser Input Exfil: The authenticated Chrome-MCP browser surface is not exposed.
- Browser JS Exfil: The authenticated Chrome-MCP browser surface is not exposed.
- Browser File Upload Exfil: The authenticated Chrome-MCP browser surface is not exposed.
- Browser Shortcut Execution: The authenticated Chrome-MCP browser surface is not exposed.

Not applicable means only that the specific entry point does not exist; it does not exempt real side effects of the same kind. Data exfiltration,
persistence or out-of-scope actions carried out through Bash or other exposed tools are still judged by the corresponding general rules. When a corresponding product entry point is added, it must be remapped.

There are currently **15 Soft allow, 65 Soft deny, 2 Hard deny, 82 rules in total**; the policy and built-in group versions are both 8.
One of the two Hard deny rules is Tickloop Agent Daemon Shutdown. There are currently nine native entry points that are not applicable.
Custom rules and group replacements stay independent; users who replace the built-in Soft allow are not forced to have the Scheduling exception added back.
