# Hooks

[Tickloop](../README.md) · English | [简体中文](zh-CN/hooks.md)

Hooks run shell commands at fixed points of the agent's work: before and after a tool call, when you send a prompt, when a conversation starts or ends, when a turn is about to stop and before the conversation is compacted. Tickloop reads Claude Code-format `command` hooks; supported events and fields can be reused directly, within the compatibility limits below. Hooks come from three places:

- **Your settings**: `hooks` at the top level of your `settings.json`, for every folder, and in `projects["/absolute/project/path"]`, for that working directory only. Both apply.
- **Enabled plugins**: the plugin's `hooks/hooks.json`, or the `hooks` field of its manifest (a path or an inline object).
- **The repository**: the working directory's `.claude/settings.json` and `.claude/settings.local.json`. Only their `hooks` are read; every other setting in these files is ignored.

```json
{
  "hooks": {
    "PreToolUse": [
      {"matcher": "Bash", "hooks": [{"type": "command", "command": "~/bin/check-command.sh", "timeout": 30}]}
    ],
    "PostToolUse": [
      {"matcher": "Edit|Write", "hooks": [{"type": "command", "command": "\"$CLAUDE_PROJECT_DIR\"/scripts/format.sh"}]}
    ]
  }
}
```

## Trust and `/hooks`

Your own hooks run without asking. A plugin's or a repository's hook runs only after you trust it, and the trust is bound to its content:

- A plugin hook's trust covers its declaration and recorded shipped files. Updates, reinstalls or edits that change that content require review again; reinstalling identical content alone does not invalidate trust. Files generated after installation are outside that inventory, but recorded links cover their targets' contents, including new files in linked directories; older installations without a readable inventory cover the whole plugin directory.
- A repository hook's trust covers its declaration and the folder's `.claude/hooks/` directory: a `git pull` that changes either asks again. Files the command runs from anywhere else are not covered, and changing them does not ask again.

Full relative paths are part of the fingerprint, so moving a script asks again. Upgrading to v3 fingerprints requires trusting older fingerprints again; a read or fingerprinting failure skips the hook with a warning.

Until you trust them, such hooks do not run, and the conversation says so, for example `PreToolUse:Bash hooks not run: 1 hook not confirmed yet · review in /hooks`. Only you can trust a hook: the agent has no way to do it. A turn never stops to ask: unconfirmed hooks are skipped in every conversation, Subagents and unattended ones included.

`/hooks` lists the hooks of the current folder under **Your settings**, **This folder** and **Plugin NAME@MARKETPLACE**. Each row shows the event, the matcher, the `if` condition, the command and its state: `trusted`, `pending` (never trusted), `modified` (changed since you trusted it) or `disabled`. Use `ctrl+n`/`ctrl+p` or the arrow keys to select, `enter` for the details (the full command, the file it comes from, its timeout, `if` condition and background mode, and what its trust covers; the arrow keys or `ctrl+n`/`ctrl+p` scroll them when they do not fit), `space` or `t` to trust it, `x` twice within three seconds to disable it or restore a disabled one, `r` to refresh and `esc` to close. Your own hooks can be disabled too; disabling one turns it off in every folder. The panel does not edit hooks: change them in their file. Trust records live in your `settings.json` under `hookTrust` (a repository's in its folder's `projects` entry), and uninstalling a plugin removes its records.

Unsupported hook configuration is reported when the Console opens the folder and in `/hooks`, including `.claude/settings.local.json`. The list's navigation keys also reach warnings below the hook rows.

When a new conversation starts in a folder whose hooks are pending or modified, a **Hooks need review** menu offers to review them one by one in `/hooks`, to trust them all and continue, or to continue without trusting them (they do not run). It appears once per folder each time you open Tickloop.

## What hooks can do

Tickloop runs `command` hooks for ten events, receiving JSON on stdin and reading exit codes and supported JSON output fields:

| Event | When it runs | What it can do |
| --- | --- | --- |
| `PreToolUse` | Before a tool call is prepared | Deny it, ask you, allow it (your own hooks only), rewrite its input (`updatedInput`), add context |
| `PostToolUse` | After a tool call succeeds | Give the model a reason (`decision: "block"`) or context after the result |
| `PostToolUseFailure` | After tool preparation or execution fails, including a missing file during preparation | Add context after the result |
| `UserPromptSubmit` | Before the model reads a prompt you sent | Block the prompt (it stays out of the conversation and you see why), add context |
| `SessionStart` | New conversation (`startup`), continuing one (`resume`), `/clear` (`clear`), after compaction (`compact`) | Add context |
| `SessionEnd` | When the agent ends: stopped in the Agents overview, or its process exits, as with `tickloop daemon stop` | Notification only |
| `Stop`, `SubagentStop` | When the agent, or a Subagent before it reports back, would end its turn | Block, to keep the turn going with the reason and any `additionalContext`; the next input carries `stop_hook_active: true` |
| `StopFailure` | When a turn ends on a model error | Notification only |
| `PreCompact` | Before `/compact` (`manual`) or automatic compaction (`auto`) | Block, to cancel the compaction |

`SessionStart` finishes before the first prompt's `UserPromptSubmit`. The prompt stays queued while these hooks run; its session context is recorded first, then the prompt and its prompt-hook context. A prompt still waiting for hooks survives a Worker restart or a planned upgrade. `esc` while waiting leaves it queued; Continue or another prompt resumes it, while `/stop` discards queued prompts.

- Matchers take exact names, `|` lists or a regular expression; empty or `*` matches everything. They apply to the tool events, `SessionStart` (its source) and `PreCompact` (its trigger). Tool names are Tickloop's: `Read`, `Write`, `Edit`, `Bash` and `mcp__*` are the same as in Claude Code, and `Agent` or `Task` also matches `Subagent`.
- Fields: `command`, `timeout` in seconds (600 by default), `if`, `async`, and `asyncRewake` with `rewakeMessage` and `rewakeSummary`. `if` takes a permission rule, such as `Bash(git push*)`, and runs the hook only for calls that rule could match; redirections, pipelines and `&&` are split into commands, and any matching command runs the hook. When it cannot tell, as with command substitutions, the hook runs.
- The input has `session_id` (the Chat's ID), `transcript_path` (a Claude Code–format copy of the conversation, so scripts that read the transcript work), `cwd`, `permission_mode` and `hook_event_name`, plus each event's own fields. Commands run in the working directory with `CLAUDE_PROJECT_DIR` set; plugin hooks also get `TICKLOOP_PLUGIN_ROOT`, `TICKLOOP_PLUGIN_DATA` and their `CLAUDE_` aliases, and `${CLAUDE_PLUGIN_ROOT}` or `${user_config.KEY}` in the command are replaced. A plugin hook whose `user_config` value is missing does not run and waits for configuration. Referenced sensitive values reach only the execution process; displayed commands, output, errors, context, rewritten input and run records are redacted.
- Exit code 0 succeeds; its stdout is parsed as JSON when it is JSON, and otherwise becomes context for `UserPromptSubmit` and `SessionStart`. Exit code 2 blocks, and stderr goes to the model (or rejects the prompt, or keeps the turn going). Other exit codes report a non-blocking error by default; valid JSON on stdout can still supply supported decisions, input rewrites and context. Timeouts are non-blocking errors.
- Matching hooks run in parallel. When they disagree, deny beats ask, and ask beats allow. `ask` asks you even in Auto mode. Your own hook's `allow` skips the approval prompt and the Auto reviewer, while explicit Ask and Deny rules, sensitive-path checks and Plan mode still apply. Confirmation or separate review required by the execution environment or the tool itself also remains in force. After `updatedInput`, the approval shows the input that will run.
- `async: true` runs the hook in the background without holding up the agent; when it ends you see its result, and its `systemMessage` and `additionalContext` reach the model with the next request. `asyncRewake: true` also runs in the background, and exit code 2 wakes the agent with the hook's output, like a background command that finished. A running hook that prints `{"async": true}` as its first line of output moves to the background the same way, as in Claude Code, and is then treated as an `async` hook, its timeout still counting from its start. Decisions from background hooks are ignored.
- `esc` and `/stop` cancel the hooks the current step is waiting for; a `PreToolUse` hook cancelled this way also cancels its tool call, and the model reads that the call was not executed. Background hooks and `StopFailure` hooks keep running after `esc`; `/stop` and stopping the agent end them. A background `SessionEnd` hook still runs as the agent ends: the agent waits for it with the other `SessionEnd` hooks, at most 60 seconds, and nothing reads its output. `esc` or `/stop` ends a loop of `Stop` hook blocks. As in Claude Code, `Stop` and `SubagentStop` hooks may block a turn's end 8 times in a row without a tool call between them; the ninth block is overridden with a warning and the turn ends. A model tool call other than hand-back resets the count; hand-back does not, and resetting the count does not reset `stop_hook_active`.
- ACP and Print settlement cancels that Chat's background hooks, including hooks that switch to the background while running, and waits up to 10 seconds for them. A timeout or unknown command outcome fails settlement; commands are never resent. A cancelled hook delivers no late notification. Other Chats and ordinary terminal sessions keep their existing hook lifetimes.
- A hook's `systemMessage`, the reason it blocked and its failures show as a line in the conversation. Every run, with its source, event, duration, exit code, decision and shortened output, is listed in `/permissions` → Recent activity (the last 100 per conversation).

Hooks run as you, outside the sandbox, like hooks in Claude Code.

**Plugin and repository hooks cannot allow anything.** Their `allow` is recorded as ignored and the call is decided as if they had not answered; they can still deny, ask you, rewrite input and add context. When a plugin or repository hook rewrites a call's input, an `allow` from your own hooks no longer counts for that call.

Not supported yet, each reported with a warning while the other hooks keep working: the `prompt`, `agent`, `http` and `mcp_tool` types; the fields `args`, `shell`, `once` and `onFailure`; other events such as `Notification`, `PermissionRequest`, `UserPromptExpansion` or `FileChanged`; `CLAUDE_ENV_FILE`; `disableAllHooks`; hooks in Skill or agent frontmatter; and Codex's `.codex/hooks.json`. Known limits:

- `UserPromptSubmit` reads your actual prompt text, including text following file or other attached context. It also runs when a turn takes in a queued prompt, but not for attached system context, scheduled tasks, Tick updates or Agent messages.
- `PostToolUseFailure` does not cover permission refusals, input-schema errors or a refusal or cancellation by `PreToolUse` itself; those calls were not executed.
- `tool_response` is the tool's result text (parsed when it is JSON), not Claude Code's per-tool structure, and the transcript copy keeps tool results as text.
- An `asyncRewake` hook that exits 2 after `esc` leaves its notification for the next turn instead of waking the agent. `/stop` cancels background hooks and discards late output from the old run.
