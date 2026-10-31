# Permissions

[Tickloop](../README.md) · English | [简体中文](zh-CN/permissions.md)

The mode decides how the agent gets permission to act. The macOS sandbox for Bash and Shells is a separate page: [Sandbox](sandbox.md).

## Execution modes

**Auto** (the default) lets a reviewer model approve routine actions and asks you about risky ones; **Ask** allows reads and edits in the working directory and asks about other protected operations; **Plan** only plans; **Full Access** skips ordinary approval but keeps explicit Ask and Deny rules. Switch with `/permissions`, `/mode` or `shift+tab`, start with `--mode MODE`, or set the mode new Chats start in with `defaultMode` in your `settings.json`.

`--permission-mode` is an alias of `--mode`; giving both is accepted only when they name the same mode. Both flags and `defaultMode` take Claude Code's names:

| Value | In Tickloop |
| --- | --- |
| `acceptEdits` (or `ask`) | Ask |
| `auto` | Auto |
| `plan` | Plan |
| `fullAccess` | Full Access |
| `default` | Ask, with a warning to use `ask` instead |
| `dontAsk` | Not a mode: use `--deny-on-prompt`, which refuses every operation that would need your approval |
| `bypassPermissions` | Not a mode: use `--dangerously-skip-permissions`, which requires external isolation |

A launch flag with an unusable value refuses to start and names the replacement; the same value in `defaultMode` produces the same note as a warning, and new Chats start in Auto.

Plan mode still refuses file edits and commands that change something, except the plan file the agent writes; that file stays writable without asking in every mode. When the agent presents a plan, approve it to start in the mode you planned from, shown on the approval option, or keep planning with feedback; `shift+tab` in the feedback field approves with that feedback. When the agent asks to leave Plan mode without a plan, answer yes to return to the mode you planned from, or no to keep planning. Either way, when that mode is unknown, for example after a restart or for a conversation that started in Plan mode, it is Ask. What it would otherwise ask you about, such as `WebFetch`, reads outside the working directory or MCP tool calls, it handles as that same mode, whether you switched to Plan or the agent entered it: after Auto mode, as Auto does, allowed directly when Auto would allow it and otherwise decided by the Auto reviewer, which asks you only when it does not allow the call; after Full Access, it allows them without asking, as Claude Code's Plan mode does when bypass permissions are available, while edits and commands that change something are still refused; after Ask mode, or when the mode is unknown, it asks you. Explicit Ask and Deny rules still take precedence, and broad shell Allow rules do not skip the review, as in Auto. To be asked after Auto mode too, set `"useAutoModeDuringPlan": false` in `settings.json`; conversations started or reopened afterwards use it.

## Rules for one launch

To add temporary tool rules for a new Agent, run `tickloop --allowedTools "Bash(git:*) Edit" --disallowedTools "Bash(rm:*)"`. The aliases `--allowed-tools` and `--disallowed-tools` also work. Repeat a flag or separate rules with spaces or commas; a rule that cannot be parsed stops the launch. These flags cannot be combined with `--continue`, `--resume` or `--chat-id`. `/permissions` lists these as read-only **Launch arguments**. They apply to that Agent and its Subagents until restart, never to settings or other Agents. Allowed tools are added to your rules, not an exclusive list: tools you do not name keep their ordinary handling. Deny and Ask rules and sensitive-path checks still take precedence; broad Bash allows still require Auto review. Add `--deny-on-prompt` to reject operations that would need human approval.

## Additional directories

Use `tickloop --add-dir ../shared --add-dir /path/to/docs` to add directories for Agents created by this launch, or `/add-dir PATH` within a Chat. For new Agents to include them by default, put `"permissions": {"additionalDirectories": ["../shared"]}` in your own `settings.json`. The same field under `projects["/absolute/project/path"]` applies only to that exact working directory and adds to the global list. Relative paths resolve from the launch working directory (`--cwd` when supplied); unavailable paths are warned about and skipped.

These sources merge into the Chat's existing additional directories, with duplicates removed. They support reads, edits and `@` file mentions; sensitive-file checks and explicit Ask/Deny rules still apply. `/cwd` shows each directory's sources. Repository settings cannot add directories, and `--add-dir` and `/add-dir` do not write them to settings.

## Importing Auto rules from Claude Code

In `/permissions` → Auto mode, choose **Import from Claude Code** to copy the `autoMode` rules from your Claude Code user settings (`~/.claude/settings.json`, or `settings.json` in `CLAUDE_CONFIG_DIR` when it is set) once. No project file is read and the Claude Code file is not changed. `allow` entries become Soft allow rules, `soft_deny` (or the older `deny`) entries Soft deny rules, and `environment` entries global Environment entries marked as imported. The preview lists what will be added and what will be skipped, and why: text already among your custom rules and rules named like a Tickloop built-in are skipped unless you select them; empty entries, entries over 4096 bytes and additions that would take a group past 256 entries cannot be saved. Soft allow entries are marked **Will relax review**. Use `Ctrl+N`/`Ctrl+P` to move, `space` to toggle entries, `enter` to save and `esc` to cancel without changes. Built-in rules stay in place; imported rules are ordinary custom rules afterwards and do not override explicit Ask or Deny. Only you can start an import; the agent has no tool for it.
