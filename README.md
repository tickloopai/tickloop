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

Then press Enter on the Counter in the Monitor, type `+` and press Enter: the agent receives the new count and responds.

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
| `/mcp` | Inspect MCP servers, manage their login and turn them on or off here |
| `/context` | See what fills the context, by category, and how many MCP tools tool search can reach; `/context all` lists every item |
| `/artifacts` | Sign in to Artifacts and add artifacts to the Chat |
| `/exit` | Leave the interface (the agent keeps running) |

Keys:

- `enter` sends a message. While the agent works it waits for the next step; `ctrl+enter` interrupts and sends right away.
- `esc` interrupts the agent. Press it twice on an empty input to undo to an earlier message.
- `shift+tab` switches between Auto and Plan mode.
- `ctrl+j` inserts a new line; `ctrl+g` edits your message in an external editor.
- In lists and menus, use `j`/`k` or the arrow keys, `enter` to choose and `esc` to go back.
- In the Monitor, `enter` opens a Tick and `esc` hides it again; the program keeps running.
- To close/remove a Tick, stop a Shell, unlink an Artifact or delete a scheduled task in Monitor, press plain `x` twice on the same row within three seconds. The first press shows a red `Press x again to …` hint, with no dialog. Expiry, selection movement, another key or leaving cancels it. `Ctrl+X` does not perform these actions; in text inputs and filters, `x` stays text.

Inside Herdr, Monitor registers a popup plugin for each configuration root. The default `~/.tickloop` keeps `tickloop.monitor` at `~/.tickloop/herdr-popup-plugin`; other roots, including `make dev`’s `~/.tickloop-dev`, use `tickloop.monitor.<root-key>` at `<configuration root>/herdr-popup-plugin`, with the root path in the display name. The stable root key is shared with the daemon and Tickhost. Both plugins can be registered in one Herdr session. On first use in each root, accept Monitor’s registration offer, or run `tickloop monitor popup install --herdr-socket SOCKET`; `remove` unregisters only that root’s plugin. Existing default-root registrations keep working.

## Instructions

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

In `/ticks`, `Tab` or left/right switches between **Installed** and **Available**. Use `j`/`k` or `Ctrl+N`/`Ctrl+P` to select. On Installed, `Enter` opens a Tick's actions: log in or out, update, enable or disable. Installed rows show the marketplace, enabled state, version and login state. Login happens in the panel: an OAuth Tick shows a link and code to approve in a browser on any device, and a key is typed masked. On Available, `Enter` installs or updates the selected Tick, and `/` starts a case-insensitive filter across names, descriptions and marketplaces: `j`/`k` enter text, `Ctrl+N`/`Ctrl+P` select, and `Ctrl+G` opens an editor. `Esc` leaves filtering and keeps the query; another `Esc` closes the panel.

To uninstall in the panel, press `x` twice on the same Installed row within three seconds. This deletes the Tick's data, keeps saved login, and leaves running Ticks running. Moving selection, another key, leaving the panel or waiting cancels confirmation. Saved logins remain under `credentials/NAME/` in your configuration directory; log out before uninstalling to remove one.

Once a Tick is installed (and logged in, if it needs a login), ask the agent to open it by name, for example "open the NAME Tick and watch it". Installation runs nothing: a Tick's program runs as you only when opened under your permission settings. A disabled Tick cannot be opened by name. Installing, updating, removing and logging in are your actions; the agent only asks you to do them.

Any program can become an Extension Tick by speaking the [Tick protocol](tick/protocol/SPECIFICATION.md); start from the [counter example](tick/examples/counter.py).

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

Your own MCP servers live in `mcpServers` in `settings.json`, for every directory, or in `projects[<directory>].mcpServers` for one; `tickloop-artifacts` and names starting with `plugin_` are reserved, and user service names containing `__` are rejected. `tickloop mcp add <name> <url> [--header 'Name: value']...` adds a remote server and `tickloop mcp add <name> [--env NAME=value]... -- <command> [arguments...]` a local one; both write the global settings, or this directory's with `--dir`. `tickloop mcp remove <name> [--dir]` removes one. `tickloop mcp list` shows every server an Agent here would have, by source, with its transport, exposure, whether it is turned off here and whether it is logged in, without starting any.

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

Without a permission rule, third-party tool calls ask for confirmation in Ask mode; in Auto mode the Auto reviewer decides and asks you only when it does not allow the call. A server calling a tool read-only changes neither. You can allow one call, always allow the tool or server in this directory, or deny it. In Plan mode the model is told to call only read-only MCP tools, and their calls are decided as in the mode Plan was entered from: Auto review after Auto, no confirmation after Full Access, otherwise a confirmation. Full Access runs them, and explicit Ask and Deny rules still take precedence. A timeout, cancellation or disconnection after dispatch can leave an unknown outcome: check the result before retrying a change. Resources use the same `ListMcpResourcesTool` and `ReadMcpResourceTool` as Artifacts.

### Tool Search

MCP tools default to `exposure: "search"`. Models whose cards support native Tool Search find tools with their own search and then call them directly. Other models use Fold: `SearchTools` finds tools from one `query` of space-separated words (any word matches; also an exact tool name, `mcp__<server>` for a server's tools, `select:A,B`, or `+word` to require a word), `DescribeTools` supplies their full parameters, and `ExecuteTools` runs 1–10 independent calls with individual approval. Searches also name servers that are still connecting, failed or need login; the first request waits up to 10 seconds for servers that are starting. Switching models selects the available form automatically. Use `direct` to list tools in every model request, or `hidden` to make them unavailable. `toolExposure` overrides individual original tool names; exact names win, then the first matching `*` pattern. Service updates append context without changing Fold tool definitions.

If the Agent's process exits while an `ExecuteTools` batch runs, the batch is not run again: the loaded history lists what each of its calls did. In the conversation, each call a batch makes shows under its `ExecuteTools` call.

Tool result rows show an Auto review in progress or a wait for your approval. Once allowed, a separate muted line below the result names the Auto reviewer or says “Approved by you”. Directly allowed calls have no approval label. Human approvals keep an extra line explaining why you were asked; denied calls show who denied them. ExecuteTools labels each nested call and summarizes the batch. Reopened conversations restore labels for the approval records still retained.

### Manage servers and log in

`/mcp` shows the Agent's servers grouped as global, this directory and each plugin, with transport, state (connected, connecting, failed, needs login, needs configuration, disabled, not supported yet), tool count and exposure. Enter on a server shows its tools, logs in again or out, reconnects it, or turns it off or on in this directory, plugin servers included (`projects[<directory>].disabledMcpServers`). Use `j`/`k` or `Ctrl+N`/`Ctrl+P` to select, `Enter` for actions, `r` to refresh and `Esc` to go back. Add and remove services with `tickloop mcp add` and `remove`; the panel manages existing entries. A login in `/mcp` shows a link and can open a browser locally; on a remote host, open it on your own computer and paste the full callback address back into the panel. `Esc` or `Ctrl+C` cancels. `/plugins` shows how many MCP servers an installed plugin carries and whether one needs a login.

For a remote MCP server that needs OAuth, run `tickloop mcp login <server>`. Open the printed link in your browser; Tickloop receives the callback locally, or you can paste the full callback address into the terminal when using SSH. Ctrl+C cancels without replacing your saved login. In terminal callback input, Ctrl+G opens your external editor. Plugin servers accept both `plugin:<plugin>:<server>` and `plugin_<plugin>_<server>`.

The login becomes available from the next Agent request and tokens renew automatically. `tickloop mcp logout <server>` revokes the token where supported and deletes it locally, keeping the client registration. Credentials are private local files under `credentials/_mcp/` or `credentials/<plugin>/mcp/`; changing a server address prevents reuse of its old token. A fixed `Authorization` header bypasses OAuth. Remote entries support `oauth.clientId`, `oauth.callbackPort`, and `authServerMetadataUrl` (also accepted inside `oauth`).

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
