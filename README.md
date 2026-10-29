# Tickloop

[Website](https://tickloop.ai) · English | [简体中文](README.zh-CN.md)

Tickloop is an AI agent that works in your terminal. Tell it what you need in plain language: it reads your project, edits code, runs tests, organizes material, writes documents and builds pages or slide decks, and keeps going until the job is done.

What makes it different are **Ticks**: live programs, such as a log viewer, a dashboard or a long-running command, that you and the agent watch together. The agent can read a Tick, use it, and be woken up when something in it changes.

## What you can do

- **Hand off real work.** The agent edits files, runs commands and checks the results. By default it works in Auto mode, where a reviewer model approves routine actions and asks you about risky ones.
- **Keep an eye on things.** Open a program as a Tick and subscribe to it; the agent reacts when it changes. The Monitor beside the conversation lists your Ticks, background commands and scheduled tasks.
- **Repeat work on a schedule.** `/loop 5m check the deploy` runs an instruction every five minutes; `/goal` sets a condition the agent keeps working toward.
- **Use the model you like.** Claude, OpenAI (API or ChatGPT subscription), GitHub Copilot, Gemini, DeepSeek, Kimi, MiniMax, GLM, xAI and Alibaba are built in. Switch at any time with `/model`.
- **Share results.** Publish docs, pages and slides to Tickloop Artifacts and get feedback from others.
- **Leave and come back.** Closing the terminal does not stop the agent. `tickloop -c` picks up the latest conversation.

## Install

macOS:

```sh
brew install tickloopai/tap/tickloop
```

macOS or Linux:

```sh
curl -fsSL https://tickloop.ai/install.sh | sh
```

With Rust installed, `cargo install --locked tickloop` also works. All three install the `tickloop` command and its short alias `tl`, and keep themselves up to date in the background. Run `tickloop update` to update right away.

## Get started

1. Run `tickloop` in your project folder.
2. On first launch, pick a model provider, enter its API key or sign in, then choose a model.
3. Describe what you want done.

Want to see a Tick in action? Ask the agent:

> Open `python3 tick/examples/counter.py` with `protocol: true` and subscribe to it.

Then select the Counter in the Monitor and press Enter to step into its card, type `+` and press Enter: the agent receives the new count and responds. Press `esc` twice to step out again.

## Everyday use

| Command | What it does |
| --- | --- |
| `/help` | List all commands |
| `/model`, `/provider` | Switch models, or sign in to another provider |
| `/plan` | Plan first, then approve before anything changes |
| `/goal`, `/loop` | Keep working toward a goal; repeat an instruction |
| `/stop` | Stop everything the agent is doing |
| `/rewind`, `/diff` | Undo back to an earlier message; review file changes |
| `/resume`, `/clear` | Reopen a past conversation; start a new one |
| `/permissions`, `/settings` | Choose how much the agent may do on its own; other preferences |
| `/ticks`, `/plugins` | Install and log in to Ticks; manage other plugins |
| `/mcp` | Inspect MCP servers, manage their login, change their exposure and turn them on or off here |
| `/context` | See what fills the context, by category, and how many MCP tools tool search can reach; `/context all` lists every item |
| `/artifacts` | Sign in to Artifacts and add artifacts to the Chat |
| `/exit` | Leave the interface (the agent keeps running) |

Keys:

- `enter` sends a message. While the agent works it waits for the next step; `ctrl+enter` interrupts and sends right away.
- `esc` interrupts the agent. Press it twice on an empty input to undo to an earlier message.
- `shift+tab` switches between Auto and Plan mode.
- `ctrl+j` inserts a new line; `ctrl+g` edits your message in an external editor.
- In lists and menus, use `ctrl+n`/`ctrl+p` or the arrow keys, `enter` to choose and `esc` to go back.
- Ticks appear as cards in the Monitor column, inside Herdr or any other terminal. In the Monitor list, `space` (or a click on the row) shows or hides a Tick's card; `enter` (or a click on an open card) steps into the Tick, so your keys go to it, `ctrl+c` included. Several cards can be open at once.
- In the Monitor list, `tab` and `shift+tab` move through every row in turn and preview the card of each Tick they pass; the preview closes when the selection moves on or you leave the list, unless you step into it (`space` hides it, as on any card). Other moves only select a row, but an open card squeezed out for lack of room shows while its row is selected. When rows with an update you have not seen are scrolled out of view, a `↑ 2 updated` or `↓ 1 updated` line says so, and clicking it jumps to the nearest one. Rows stay in the order they joined, so an update never moves them.
- Inside a card, a single `esc` goes to the Tick; `esc esc` steps back to the list and the card stays open. Ticks that support it hand back keys they do not use: then `esc` alone steps out, the up and down arrows step out to the list with the card above or below selected, `tab` and `shift+tab` step out and move through the list as they do there, and left goes back to your input. Stepping into another card still takes `enter` or a click. Clicking outside the card also steps out, and the click then does what it does there: on another Tick's row it shows that Tick's card. Shell cards are read only, and `esc` steps out of them. The program keeps running when you hide its card or step out.
- When the agent uses a Tick or reads its update, the Tick's card opens by itself, unless you hid it.
- When a Tick ends, its card keeps the last screen and its rule says how, such as `exited 101` or `ended`.
- `←` on an empty input opens the Agents overview. As in Claude Code's agent view, Agents are grouped under Needs input (with how long each has waited), Working and Completed; Completed holds Idle, Completed, Failed and Stopped Agents, newest end first, and folds what the screen cannot hold behind `… N more`. An Agent that holds nothing is not listed. Agents whose process ended stay listed until `cleanupPeriodDays` (30 days by default) passes, and nothing restarts them. One whose turn had finished with nothing pending stays Completed, and one that ended on a question or plan for you stays in Needs input; one that was working, or idle with work still pending (a Goal, `/loop` or scheduled task, a Tick Subscription, a background Shell or Subagent), shows Failed when the daemon stopped within 48 hours of its last activity and Stopped when it was longer. `enter` on such a row loads its saved conversation without running anything, and your next message continues it; a row that ended while the daemon was off asks for a second `enter`. `space` on a row opens its peek panel: the Agent's state, its latest sentence and what it waits on, with a reply box below. `enter` sends the reply without leaving the overview (a number answers the Agent's question when every option is on screen; a permission request or plan approval is answered only in the Agent's own view; replying to an ended row loads it first), `↑`/`↓` move the panel to the next Agent, `→` opens the Agent and `esc` (or `space` on an empty reply) closes the panel; `enter` or `→` folds a group heading. `x` twice on a running Agent stops it (it stays listed as Stopped); `x` twice on an ended row removes the row and keeps the conversation. `r` (or `/resume`) in the overview lists the saved conversations not in the list yet and adds the one you choose.
- To close/remove a Tick, stop a Shell, unlink an Artifact or delete a scheduled task in Monitor, press plain `x` twice on the same row within three seconds. The first press shows a red `Press x again to …` hint, with no dialog. Expiry, selection movement, another key or leaving cancels it. `Ctrl+X` does not perform these actions; in text inputs and filters, `x` stays text.

When a conversation nears the end of the model's context window, the agent summarizes it before its next request. That happens once the context passes the window less room for the summary (the model's output limit, at most 20,000 tokens) and a 13,000-token buffer, but never below half the window; `/context` shows the space past that point as the Autocompact buffer. From 20,000 tokens before that point the status line's `Context N% used` turns to the warning color and the conversation says once how near it is; `/compact` compacts at any time. To compact earlier, set `autoCompactWindow` in `settings.json` to a number of tokens, for example `"autoCompactWindow": 400000`; when it is smaller than the model's window, the point is measured in it instead. Conversations started or reopened afterwards use it.

## Permissions and sandbox

### Execution modes

The mode decides how the agent gets permission to act. **Auto** (the default) lets a reviewer model approve routine actions and asks you about risky ones; **Ask** allows reads and edits in the working directory and asks about other protected operations; **Plan** only plans; **Full Access** skips ordinary approval but keeps explicit Ask and Deny rules. Switch with `/permissions`, `/mode` or `shift+tab`, start with `--mode MODE`, or set the mode new Chats start in with `defaultMode` in your `settings.json`.

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

### Rules for one launch

To add temporary tool rules for a new Agent, run `tickloop --allowedTools "Bash(git:*) Edit" --disallowedTools "Bash(rm:*)"`. The aliases `--allowed-tools` and `--disallowed-tools` also work. Repeat a flag or separate rules with spaces or commas; a rule that cannot be parsed stops the launch. These flags cannot be combined with `--continue`, `--resume` or `--chat-id`. `/permissions` lists these as read-only **Launch arguments**. They apply to that Agent and its Subagents until restart, never to settings or other Agents. Allowed tools are added to your rules, not an exclusive list: tools you do not name keep their ordinary handling. Deny and Ask rules and sensitive-path checks still take precedence; broad Bash allows still require Auto review. Add `--deny-on-prompt` to reject operations that would need human approval.

### Additional directories

Use `tickloop --add-dir ../shared --add-dir /path/to/docs` to add directories for Agents created by this launch, or `/add-dir PATH` within a Chat. For new Agents to include them by default, put `"permissions": {"additionalDirectories": ["../shared"]}` in your own `settings.json`. The same field under `projects["/absolute/project/path"]` applies only to that exact working directory and adds to the global list. Relative paths resolve from the launch working directory (`--cwd` when supplied); unavailable paths are warned about and skipped.

These sources merge into the Chat's existing additional directories, with duplicates removed. They support reads, edits and `@` file mentions; sensitive-file checks and explicit Ask/Deny rules still apply. `/cwd` shows each directory's sources. Repository settings cannot add directories, and `--add-dir` and `/add-dir` do not write them to settings.

### Importing Auto rules from Claude Code

In `/permissions` → Auto mode, choose **Import from Claude Code** to copy the `autoMode` rules from your Claude Code user settings (`~/.claude/settings.json`, or `settings.json` in `CLAUDE_CONFIG_DIR` when it is set) once. No project file is read and the Claude Code file is not changed. `allow` entries become Soft allow rules, `soft_deny` (or the older `deny`) entries Soft deny rules, and `environment` entries global Environment entries marked as imported. The preview lists what will be added and what will be skipped, and why: text already among your custom rules and rules named like a Tickloop built-in are skipped unless you select them; empty entries, entries over 4096 bytes and additions that would take a group past 256 entries cannot be saved. Soft allow entries are marked **Will relax review**. Use `Ctrl+N`/`Ctrl+P` to move, `space` to toggle entries, `enter` to save and `esc` to cancel without changes. Built-in rules stay in place; imported rules are ordinary custom rules afterwards and do not override explicit Ask or Deny. Only you can start an import; the agent has no tool for it.

### Sandbox (macOS)

On macOS, Bash, background Shells and `!` Shells can run in an OS sandbox. It is off by default and is read only from your own `settings.json`; repository content cannot turn it on, off or widen it. For example:

Use `/sandbox` to choose **Mode** (sandbox with auto-allow, sandbox with regular permissions, or off), **Overrides** (allow escape requests or strict sandbox), and the read-only **Config** page. Left/right switches pages; `Ctrl+N`/`Ctrl+P` or up/down moves or scrolls, Enter saves, and Esc closes. `/sandbox exclude "docker:*"` adds an exclusion for this Folder; excluded commands still use normal approval. Non-macOS hosts can inspect settings but cannot enable the sandbox through this command.

Changes are saved only in your own `settings.json`, under `projects["<canonical Folder>"].sandbox`, and apply to the next command without restarting. Folder scalars override global `sandbox` settings; `excludedCommands`, `filesystem.allowWrite`/`denyWrite`, and `network.allowedDomains` merge and deduplicate across both scopes. Config labels each setting's source and shows writable scope, protected paths, network rules and host support. Concurrent sandbox edits refresh the page instead of overwriting the other edit. `/permissions` links back to `/sandbox`.

```json
{
  "sandbox": {
    "enabled": true,
    "network": { "allowedDomains": ["registry.npmjs.org", "*.github.com"] }
  }
}
```

Sandboxed commands may write the Folder, additional directories, verified linked worktrees, the system temporary directory and the family's temp space (shared by an Agent and its Subagents). They cannot write your configuration directory, your settings file or sensitive paths such as Git hooks, and networking is blocked unless the allowlist names the destination. `/permissions` shows whether the sandbox is on. Ticks, MCP servers and the file tools (such as `Read`, `Write` and `Edit`) do not run in the sandbox; your permission rules govern them as before.

| Field | Default | Effect |
| --- | --- | --- |
| `enabled` | `false` | Run Bash, background Shells and `!` Shells in the sandbox |
| `failIfUnavailable` | `false` | When the sandbox is unavailable (another platform, or already inside a sandbox): `false` warns and runs without isolation, `true` refuses to run |
| `autoAllowBashIfSandboxed` | `true` | In Ask and Auto, sandboxed Bash and background Shell commands run without asking or Auto review |
| `allowUnsandboxedCommands` | `true` | Allow model requests through `dangerouslyDisableSandbox`; `false` ignores those requests, while `excludedCommands` still applies |
| `excludedCommands` | `[]` | Commands that always run outside the sandbox, with ordinary approval |
| `filesystem.allowWrite` | `[]` | More writable paths |
| `filesystem.allowGitConfig` | `false` | Permit `.git/config` writes; Git hooks and other protections remain in force |
| `filesystem.denyWrite` | `[]` | Paths kept read-only; wins over `allowWrite` |
| `network.allowedDomains` | `[]` | Destinations reachable through the local proxy |

Other `sandbox` fields produce an unsupported-setting warning. If the user settings file cannot be read or parsed, or its `sandbox` value is invalid, Shell preparation fails until you fix it. A valid file without `sandbox` keeps it off.

While the sandbox is on, Ask and Auto run sandboxed Bash and background Shell commands, a Subagent's included, without asking or Auto review: the sandbox, not the reviewer, bounds what they can write and reach. Explicit Ask and Deny rules still apply first, Plan and Full Access are unchanged, and `autoAllowBashIfSandboxed: false` keeps the isolation but restores ordinary approval.

For a single call, Bash can request `dangerouslyDisableSandbox: true`. With the sandbox enabled, that call runs without isolation and goes through ordinary approval; Auto reviews it instead of using the read-only or local-edit fast paths. `sandbox.excludedCommands` selects calls that should use the same route: exact commands, prefixes such as `"docker:*"`, and `*` patterns are supported. A matching part of a compound command selects the whole invocation. Leading literal environment assignments and safe wrapper commands are recognized. Setting `allowUnsandboxedCommands` to false ignores model escape requests; explicitly excluded commands still run outside the sandbox with ordinary approval. With the sandbox off, the Bash parameter has no effect.

Default write protection applies at every depth under writable roots, regardless of filename case: `.gitconfig`, `.gitmodules`, `.bashrc`, `.bash_profile`, `.zshrc`, `.zprofile`, `.profile`, `.ripgreprc`, `.mcp.json`, `.vscode/`, `.idea/`, `.git/hooks/` and `.git/config`. It also covers `.tickloop/`, `AGENTS.md`, `CLAUDE.md`, `CLAUDE.local.md`, and `.claude/` settings (`settings.json`, `settings.local.json`), skills, hooks, commands, agents and rules. Credential paths, the user's settings file and configuration root remain protected. Home files outside writable roots cannot be written either. Ordinary project files and other Git data remain writable; `filesystem.allowGitConfig: true` permits `.git/config` only, leaving hooks and explicit `denyWrite` restrictions protected.

`filesystem.allowWrite` and `denyWrite` take literal paths: relative paths resolve from the Chat Folder, independently of Bash `cwd`, and `~/` from your home directory. A `cwd` outside the Folder and additional directories goes through ordinary approval while the sandbox keeps the same writable roots. Deny rules and sensitive paths take precedence. Reads are not restricted, except that sandboxed commands cannot traverse sensitive symlinks, even to read; this prevents aliases from bypassing write protection. Configured deny paths and the user settings file also protect their resolved targets.

HTTP and HTTPS (CONNECT) go through a local proxy that sandboxed commands receive in the standard proxy environment variables (`HTTP_PROXY`, `HTTPS_PROXY`, `ALL_PROXY`); commands that ignore them cannot connect directly. Wildcards such as `*.github.com` match subdomains, not the domain itself. A denied destination is named in the command output, so you know which domain to add. One authenticated loopback proxy stays bound for the lifetime of each Tickhost; each Shell keeps the allowlist it started with.

A failed sandboxed command whose output indicates a permission denial gets a note that the sandbox probably blocked it, which suggests `dangerouslyDisableSandbox` only when escapes are allowed. Its exit status is kept, and no command is retried automatically.

Known limitation: a detached descendant can outlive a Tickhost restart. Its OS sandbox still permits the former proxy's loopback port, so it may reach a different service that later binds that port.

## Noninteractive execution and ACP

```sh
tickloop -p "Summarize this project"                 # --print is an alias; --p is not supported
printf '%s' 'Explain this text' | tickloop -p -
tickloop -p "Review this change" --output-format json --timeout 300
tickloop -p "Run the checks" --output-format stream-json
tickloop acp                                        # ACP JSON-RPC over stdin/stdout
tickloop acp --mode fullAccess                      # sessions may select Full Access (also the starting mode)
```

Configure the provider with `/provider` first. These clients use the same saved model, permission rules and Agent runtime as the Console, without opening a terminal UI. An unknown working directory is refused: trust it interactively first, or explicitly assert trust for this invocation with `--working-directory-is-trusted` (does not save trust).

Print mode defaults to Auto with human prompts denied, not Full Access. `text` writes only the final answer; `json` writes a versioned result; `stream-json` writes versioned JSONL events and a final result. Diagnostics go to stderr. Exit codes are 0 (success), 1 (execution/unknown outcome/cleanup failure), 2 (arguments), 3 (human input required), 124 (timeout), 130 (SIGINT), and 143 (SIGTERM). `--no-session-persistence` disables saving the new Chat; Print does not resume an existing Chat.

The initial managed profiles do not offer structured questions, Plan approvals, Subagents, independent/scheduled goals, Tick subscriptions or persistent Artifact feedback. Ordinary tool denials need not fail the whole task. A completed turn is settled and its remaining Shells and hosted Ticks are cleaned before success is reported; failures and unknown outcomes are not silently retried. Client crashes are covered by an expiring daemon control lease. Ordinary Console detach behavior is unchanged.

Configure an ACP-capable editor to launch `tickloop acp`. Stable ACP v1 supports successive prompts, text/tool updates, live thinking (up to 1 MiB per turn, then cut off), the session title and context usage (sent when changed at the start and end of each turn and on load), one-time permission answers, cancellation, resource references/embedded text, and inline images when the selected model supports them. Resource URIs are provenance, not permission to read or fetch them. Session-scoped configuration offers the real models of every signed-in provider (grouped by provider when there are several, as in `/model`), Auto/Ask within the startup ceiling (plus Full Access when launched with `--mode fullAccess`; `--mode` is both the starting mode and the ceiling, default auto), and the selected model's effort levels (auto or the levels it offers; session-only, never written to settings, and back to the settings default on load); advertised slash commands are actual user-invocable Skills. Persistent ACP sessions support listing and exclusive loading with public history replay; loading never runs a model, tool or MCP server and cannot take over a live Chat or ordinary Console history. Print still has no resume mode.

Client-provided stdio MCP servers are validated at session creation and start only on the first prompt, after mandatory one-time human approval (explicit Deny still wins, including in Full Access). Servers use a clean minimal environment plus explicitly supplied variables, and remain available across prompts until release/EOF/lease expiry. argv/env are not recorded in public history; argv is shown transiently to the supplying client for approval, so put secrets in env, not argv. MCP processes are not sandboxed; a supervisor cleans their process groups, but cannot guarantee cleanup of malicious `setsid` escapes or simultaneous supervisor termination. MCP tools require flat object input schemas; top-level composition and references are rejected. HTTP/SSE MCP and client filesystem/terminal execution are not supported.

A client's `cwd` is not itself a trust assertion. Prompts are bounded to 64 blocks, 256 KiB of text, 1 MiB of embedded resource text and four images (16 MiB aggregate base64); protocol frames and queues are also bounded. stdout is exclusively ACP protocol traffic. ACP is an AgentAccess client, not a Tick type.

## Instructions

Use `tickloop --help` for CLI options.

The agent follows standing instructions from Markdown files, read in this order (later files weigh more):

1. Your own instructions for every project: `~/.tickloop/AGENTS.md`. Without it, `~/.claude/CLAUDE.md` and `~/.claude/rules/*.md` are used instead.
2. Each directory from the top of the filesystem down to the working directory: its `AGENTS.md`, or, in a directory without one, its `CLAUDE.md`, `.claude/CLAUDE.md` and `.claude/rules/*.md`. An `AGENTS.md` in a parent folder therefore applies to every project below it.
3. A directory's `CLAUDE.local.md`, your private notes that stay out of Git, is always read.

To keep a directory's `CLAUDE.md` from being read, put an `AGENTS.md` there; an empty one is enough. Likewise, any `~/.tickloop/AGENTS.md`, even an empty one, keeps `~/.claude` from being read. A rule file with `paths` in its frontmatter is left out. Write `@path` to bring in another file (`@docs/style.md`, `@~/notes.md`); a project's files can only bring in files inside the working directory, while your own instructions can bring in any file. Files are read when a conversation starts and again after `/compact`.

To reuse instructions written for another tool, bring them in from `~/.tickloop/AGENTS.md` (for example `@~/.codex/AGENTS.md`), or add an `AGENTS.md` to the project that brings in its existing file (for example `@GEMINI.md`).

## Ticks

A Tick is a live program the agent can open, watch and act on, such as a market data feed or a document reader. Ticks are installed like plugins and share their marketplaces and storage, but you manage them on their own: open `/ticks` to find, install and log in to them, or use the command line:

```sh
tickloop tick list --available
tickloop tick install NAME                # or NAME@MARKETPLACE
tickloop tick list
tickloop tick login NAME                  # when the data source requires login
tickloop tick logout NAME
tickloop tick update NAME                 # omit NAME to update all Ticks
tickloop tick disable NAME
tickloop tick enable NAME
tickloop tick uninstall NAME              # deletes its data; keeps saved login
tickloop tick uninstall NAME --keep-data  # keeps its data too
tickloop tick install --path ./my-tick    # a local Tick directory or .tar.gz archive
```

Official Ticks come from the built-in `tickloop` marketplace; their sources live in [tickloopai/ticks](https://github.com/tickloopai/ticks). A plugin that carries a Tick program is a Tick, even when it also brings Skills: once installed it appears only in `/ticks` and `tickloop tick`, never in `/plugins`. When the kind is known before installing (a Tick marketplace package, or a local directory), `tickloop plugin` refuses a Tick and names the `tickloop tick` command to use instead, and the other way round.

In `/ticks`, `Tab` or left/right switches between **Installed** and **Available**. Use `Ctrl+N`/`Ctrl+P` or up/down to select. On Installed, `Enter` opens a Tick's actions: log in or out, update, enable or disable. Installed rows show the marketplace, enabled state, version and login state. Login happens in the panel: an OAuth Tick shows a link and code to approve in a browser on any device, and a key is typed masked. On Available, `Enter` installs or updates the selected Tick, and `/` starts a case-insensitive filter across names, descriptions and marketplaces: letters enter text, `Ctrl+N`/`Ctrl+P` select, and `Ctrl+G` opens an editor. `Esc` leaves filtering and keeps the query; another `Esc` closes the panel.

To uninstall in the panel, press `x` twice on the same Installed row within three seconds. This deletes the Tick's data, keeps saved login, and leaves running Ticks running. Moving selection, another key, leaving the panel or waiting cancels confirmation. Saved logins remain under `credentials/NAME/` in your configuration directory; log out before uninstalling to remove one.

Once a Tick is installed (and logged in, if it needs a login), ask the agent to open it by name, for example "open the NAME Tick and watch it". Installation runs nothing: a Tick's program runs as you only when opened under your permission settings. A disabled Tick cannot be opened by name. Installing, updating, removing and logging in are your actions; the agent only asks you to do them.

Any program can become an Extension Tick by speaking the [Tick protocol](tick/protocol/SPECIFICATION.md); start from the [counter example](tick/examples/counter.py). A Tick is shown in a Monitor card at least 30 columns wide: it must stay usable at that width, redraw its whole screen when resized, and can ask for 6 to 14 rows with `screen.min_rows` in its description (6 by default) and for more when the Monitor has room with `screen.max_rows`. A Tick may use `esc` but must not need a double `esc`, which steps out of the card. It should hand back the keys it does not use by writing `\x1b]7777;tickloop;bubble=on\x07` once at startup and `\x1b]7777;tickloop;key=<name>\x07` (`esc`, `up`, `down`, `left`, `right`, `tab`, `backtab`) for each unused key; see the specification. In Neovim, for example:

```lua
vim.api.nvim_create_autocmd('VimEnter', {
  callback = function() io.stdout:write('\27]7777;tickloop;bubble=on\7') end,
})
vim.keymap.set('n', '<Esc>', function() io.stdout:write('\27]7777;tickloop;key=esc\7') end)
```

## Plugins

Plugins add Skills and MCP servers to the agent. Open `/plugins` to browse the installed and available plugins without a Tick program, or use the command line:

```sh
tickloop plugin list --available
tickloop plugin install NAME@MARKETPLACE
tickloop plugin list
tickloop plugin update NAME                 # omit NAME to update all marketplace plugins
tickloop plugin disable NAME
tickloop plugin enable NAME
tickloop plugin uninstall NAME              # deletes plugin data
tickloop plugin uninstall NAME --keep-data  # keeps plugin data too
```

The built-in marketplaces are `tickloop` and `claude-plugins-official`. `tickloop` cannot be removed. `claude-plugins-official` is registered by default and downloads on first use; removing it is remembered across restarts. Add it back with `tickloop plugin marketplace add anthropics/claude-plugins-official`. Both names remain reserved for their official sources. Install with just `NAME` when its source is unambiguous; if several marketplaces offer it, the command lists candidates (or uses the source of an existing installation). To switch an installed name to another marketplace, uninstall it first. A plugin from a Git marketplace whose Tick program shows only once installed is installed by `tickloop plugin`, then reported and managed as a Tick.

Register other sources on the **Marketplaces** page of `/plugins` or from the command line; `/ticks` and `/plugins` both read them:

```sh
tickloop plugin marketplace add OWNER/REPO
tickloop plugin marketplace list
tickloop plugin marketplace update MARKETPLACE  # omit the name to refresh all
tickloop plugin marketplace remove MARKETPLACE
```

`add` also accepts `OWNER/REPO#REF`, Git URLs (HTTPS, SSH or file), a local directory, or an HTTPS marketplace JSON URL. It uses the name declared by the marketplace. Removing a marketplace keeps its installed plugins. Browsing uses the last fetched Git catalog; refresh it with `marketplace update`. The `tickloop` catalog is read on every use. Plugin `update` refreshes its source and checks Git commits, so it can update content even when the displayed version stays the same. Update-all skips plugins from removed marketplaces.

For a local plugin directory or `.tar.gz` archive:

```sh
tickloop plugin validate ./my-plugin
tickloop plugin install --path ./my-plugin
```

Local-path installations are updated only by running `install --path` again, even at the same version; `update` and the panels do not update them. `validate` reports the manifest, components and path warnings; installation additionally checks the file tree. A failed installation leaves the previous installation intact. If a plugin is marked unusable, reinstall it from its original source; an unusable plugin whose kind cannot be read is listed in both `/ticks` and `/plugins`.

`/plugins` works like `/ticks`: Installed and Available pages, `Enter` for actions (enable/disable, update) or to install, `/` to filter, and `x` twice to uninstall. Installed rows also show unsupported components. A failed marketplace has its own error row, summarized in at most three lines; other sources and Installed remain usable.

The third page of `/plugins`, **Marketplaces**, does what `tickloop plugin marketplace` does. Each row shows the name, source, when it was last updated and whether it is built in; a marketplace that cannot be read shows its error under its row. `a` opens a one-line field that takes the same sources as `marketplace add`; `Enter` adds, `Ctrl+G` opens an editor and `Esc` cancels. `Enter` on a row reads that marketplace again, like `marketplace update NAME`. `x` twice on the same row within 3 seconds removes it and keeps the plugins installed from it; `tickloop` cannot be removed, so its row has no `x remove` hint and ignores `x`. The default `claude-plugins-official` row can be removed and the notice explains how to add it back. Adding, updating and removing run in the background, and the Available page follows. `/ticks` has no Marketplaces page and points to `/plugins`.

Type `/` to discover user-invocable Skills for the current folder, including built-in, user, project and enabled plugin and Tick Skills. The Skills group shows each name, description and source; use `Ctrl+N` / `Ctrl+P` to select, then `Enter` or `Tab` to complete. Completion refreshes when Skills or plugins change. Invoke `/plugin:skill [ARGUMENTS]`, or `/skill [ARGUMENTS]` when that short name is unique. Ambiguous short names list the full names to use; exact names win, and built-in commands always take precedence. Each invocation displays a `Skill · name · source` row, preserved when reopening history.

Enabled plugin Skills appear as `plugin:skill` from the next model request, without restarting. Disabling hides its Skills; reinstalling or updating preserves the disabled state. Installation places files; enabled MCP servers start with the Agent or on its next request. Plugin Skill and MCP tool calls still require the usual approval. Skills, Tick programs, MCP servers and plugin user configuration are supported; other components are listed as unsupported. npm and pip plugin sources are listed but cannot be installed. Installation, updates, removal and login are user actions, not actions the model initiates.

### MCP servers

Enabled plugins can declare MCP servers in `.mcp.json` (a server map or an `mcpServers` wrapper), or in the manifest's `mcpServers` object, relative path or array. Stdio and streamable HTTP are supported. Tools are named `mcp__plugin_<plugin>_<server>__<tool>`; name components collapse consecutive underscores; long or colliding names receive a hash suffix while retaining the server/tool separator. Colliding plugin service names are skipped with a warning. `validate`, installation and enablement list the commands or addresses and any missing configuration. Enabling consents to running the servers as your user from the next Agent request; disabling or uninstalling stops them from the next request. Tool calls still follow your permissions.

Your own MCP servers live in `mcpServers` in `settings.json`, for every directory, or in `projects[<directory>].mcpServers` for one; `tickloop-artifacts` and names starting with `plugin_` are reserved, and user service names containing `__` are rejected. `tickloop mcp add <name> <url> [--header 'Name: value']...` adds a remote server and `tickloop mcp add <name> [--env NAME=value]... -- <command> [arguments...]` a local one; both write the global settings, or this directory's with `--dir`. `--exposure direct|search|hidden` sets how the server's tools reach the model; without it the entry has no `exposure` and uses `search`. `tickloop mcp remove <name> [--dir]` removes one. `tickloop mcp list` shows every server an Agent here would have, by source, with its transport, exposure, whether it is turned off here and whether it is logged in, without starting any.

For example, merge this into your configuration directory's `settings.json`, replacing the command and URL with your server's values:

```json
{
  "mcpServers": {
    "local": {
      "type": "stdio",
      "command": "my-mcp-server",
      "args": []
    },
    "remote": {
      "type": "http",
      "url": "https://mcp.example.com/mcp"
    }
  }
}
```

Directory settings override global servers of the same name. User server names use letters, digits, `_` and `-`. Repository `.mcp.json` files are not loaded. Remote URLs require HTTPS; HTTP is allowed only for loopback addresses. Legacy SSE and WebSocket transports are listed as unsupported. Optional `timeout` is the tool-call limit in milliseconds (default 300000; values below 1000 are ignored). Servers run per root Agent and are shared with its subagents; they are not Ticks and do not appear in Monitor.

Without a permission rule, third-party tool calls ask for confirmation in Ask mode; in Auto mode the Auto reviewer decides and asks you only when it does not allow the call. A server calling a tool read-only changes neither. You can allow one call, always allow the tool or server in this directory, or deny it. In Plan mode the model is told to call only read-only MCP tools, and their calls are decided as in the mode Plan was entered from: Auto review after Auto (a confirmation with `useAutoModeDuringPlan` off), no confirmation after Full Access, otherwise a confirmation. Full Access runs them, and explicit Ask and Deny rules still take precedence. A timeout, cancellation or disconnection after dispatch can leave an unknown outcome: check the result before retrying a change. Resources use the same `ListMcpResourcesTool` and `ReadMcpResourceTool` as Artifacts.

### Tool Search

MCP tools default to `exposure: "search"`. Models whose cards support native Tool Search find tools with their own search and then call them directly. Other models use Fold: `SearchTools` finds tools from one `query` of space-separated words (any word matches; also an exact tool name, `mcp__<server>` for a server's tools, `select:A,B`, or `+word` to require a word), `DescribeTools` supplies their full parameters, and `ExecuteTools` runs 1–10 independent calls with individual approval. Searches also name servers that are still connecting, failed or need login. The first request waits at most 2 seconds in total for servers that are still connecting, and the status line names them meanwhile; a server that connects later can be found from the next request, and its directly listed tools join the tool list at a later turn. A remote server that disconnects or fails to connect tries again after 1, 2, 4, 8 and 16 seconds and then stays failed; a local server that fails or exits is not started again until you reconnect it in `/mcp`, change its configuration or restart the Agent. While a server reconnects or renews its login, its last listed tools stay available. Switching models selects the available form automatically. Use `direct` to list tools in every model request, or `hidden` to make them unavailable. `toolExposure` overrides individual original tool names; exact names win, then the first matching `*` pattern. Service updates append context without changing Fold tool definitions.

If the Agent's process exits while an `ExecuteTools` batch runs, the batch is not run again: the loaded history lists what each of its calls did. In the conversation, each call a batch makes shows under its `ExecuteTools` call.

Tool result rows show an Auto review in progress or a wait for your approval. Once allowed, a separate muted line below the result names the Auto reviewer or says “Approved by you”. Directly allowed calls have no approval label. Human approvals keep an extra line explaining why you were asked; denied calls show who denied them. ExecuteTools labels each nested call and summarizes the batch. Reopened conversations restore labels for the approval records still retained.

### Manage servers and log in

`/mcp` shows the Agent's servers grouped as global, this directory and each plugin, with transport, state (connected, connecting, failed, needs login, needs configuration, disabled, not supported yet), tool count and exposure. Enter on a server shows its tools, logs in again or out, reconnects it, turns it off or on in this directory, plugin servers included (`projects[<directory>].disabledMcpServers`), or changes its exposure. After a login or Reconnect the server connects at once and its row shows `connecting` until it has listed its tools (`connected · N tools`) or failed, with the cause under its actions. Before the Agent starts, opening the panel connects remote servers to list their tools and closes those connections again; local (stdio) servers show `starts with the Agent` and connect only through Connect or `r`. `e` on a server moves its exposure from `search` to `direct` to `hidden` and back to `search`, saving each step at once. A user server's `exposure` is written in the layer that configures it, the directory's when both layers have the name, and `search` removes the field; a plugin server's is kept in the global `pluginMcpServerExposure` object by its full name (for example `"pluginMcpServerExposure": {"plugin_docs_search": "direct"}`), which overrides the plugin's own `exposure` and outlives plugin updates. A running Agent offers the new exposure from its next request, even in the middle of a turn. Use `Ctrl+N`/`Ctrl+P` or up/down to select, `Enter` for actions, `e` for exposure, `r` to refresh and `Esc` to go back. Add and remove services with `tickloop mcp add` and `remove`; the panel manages existing entries, and `toolExposure` is edited in `settings.json`. A login in `/mcp` shows a link and can open a browser locally; on a remote host, open it on your own computer and paste the full callback address back into the panel. `Esc` or `Ctrl+C` cancels. `/plugins` shows how many MCP servers an installed plugin carries and whether one needs a login.

For a remote MCP server that needs OAuth, run `tickloop mcp login <server>`. Open the printed link in your browser; Tickloop receives the callback locally, or you can paste the full callback address into the terminal when using SSH. Ctrl+C cancels without replacing your saved login. In terminal callback input, Ctrl+G opens your external editor. Plugin servers accept both `plugin:<plugin>:<server>` and `plugin_<plugin>_<server>`.

A login in `/mcp` connects the server right away, through the running Agent or, before it starts, for the panel; otherwise a login becomes available from the next Agent request. Tokens renew automatically. `tickloop mcp logout <server>` revokes the token where supported and deletes it locally, keeping the client registration. Credentials are private local files under `credentials/_mcp/` or `credentials/<plugin>/mcp/`; changing a server address prevents reuse of its old token. A fixed `Authorization` header bypasses OAuth. Remote entries support `oauth.clientId`, `oauth.callbackPort`, and `authServerMetadataUrl` (also accepted inside `oauth`).

Service configuration expands `${VAR}` and `${VAR:-default}` from the Agent's startup environment; a missing variable without a default prevents that server from starting. Login in `/mcp` or the CLI uses that process's environment. If a URL variable differs from the Agent's environment, login is saved for a different address and the Agent cannot use it: use matching values in both terminals. The model asks you to add, configure or log in to servers; it does not do those steps itself.

### Plugin user configuration

Plugins may declare `userConfig` fields (string, number, boolean, directory or file). Installation and enablement ask for missing values in a terminal; scripts receive a `tickloop plugin configure NAME` hint. Use that command or the Configure action in `/plugins` to change values later. Sensitive input is hidden or masked, does not open an external editor with `Ctrl+G`, and is saved in `credentials/NAME/user-config.json` with owner-only access; other values live in `plugins.NAME.options` in `settings.json`. Uninstalling preserves both. MCP configuration receives the values; Skills receive public values and a placeholder for sensitive values. Changes take effect on the next request.

User-configured servers take priority over plugin servers with the same command and arguments or URL. Between plugins, the first plugin by name wins; skipped duplicates produce a warning. Plugin root and data placeholders (`${TICKLOOP_PLUGIN_ROOT}`, `${TICKLOOP_PLUGIN_DATA}` and their `CLAUDE_` aliases), plus startup environment variables `${VAR}` and `${VAR:-default}`, are expanded when connecting. Services referencing missing `${user_config.*}` values show "needs configuration" and do not start. HTTP authentication is discovered on connection; validation alone cannot tell whether login is required.

Existing Tick packages migrate automatically on first use, keeping saved login. Broken packages are reported for reinstallation; the old directory is preserved.

## Requirements

- macOS (Apple silicon or Intel) or Linux (ARM64 or x86_64).
- A terminal with true color and UTF-8. Ghostty, kitty, WezTerm or iTerm2 enable extra keys such as `ctrl+enter`.

Settings, logins and history are kept in `~/.tickloop`.

## Contributing

Bug reports, ideas and pull requests are welcome; see [CONTRIBUTING.md](CONTRIBUTING.md). To build from source, see its development setup.

## Security

Please report vulnerabilities privately as described in [SECURITY.md](SECURITY.md), not in public issues.

## License

[MIT](LICENSE). Third-party code and licenses are listed in [THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md).
