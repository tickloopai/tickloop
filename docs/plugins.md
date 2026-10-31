# Plugins

[Tickloop](../README.md) · English | [简体中文](zh-CN/plugins.md)

Plugins add Skills, [MCP servers](mcp.md) and [hooks](hooks.md) to the agent. Open `/plugins` to browse the installed and available plugins without a Tick program, or use the command line:

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

## Marketplaces

Register other sources on the **Marketplaces** page of `/plugins` or from the command line; `/ticks` and `/plugins` both read them:

```sh
tickloop plugin marketplace add OWNER/REPO
tickloop plugin marketplace list
tickloop plugin marketplace update MARKETPLACE  # omit the name to refresh all
tickloop plugin marketplace remove MARKETPLACE
```

`add` also accepts `OWNER/REPO#REF`, Git URLs (HTTPS, SSH or file), a local directory, or an HTTPS marketplace JSON URL. It uses the name declared by the marketplace. Removing a marketplace keeps its installed plugins. Browsing uses the last fetched Git catalog; refresh it with `marketplace update`. The `tickloop` catalog is read on every use. Plugin `update` refreshes its source and checks Git commits, so it can update content even when the displayed version stays the same. Update-all skips plugins from removed marketplaces. A Git source with a `sha` installs that commit even if its `ref` has moved; the ref is only a fetch hint. A missing commit fails installation and leaves the existing plugin unchanged.

## Local plugins

For a local plugin directory or `.tar.gz` archive:

```sh
tickloop plugin validate ./my-plugin
tickloop plugin install --path ./my-plugin
```

Local-path installations are updated only by running `install --path` again, even at the same version; `update` and the panels do not update them. `validate` reports the manifest, components and path warnings; installation additionally checks the file tree. A failed installation leaves the previous installation intact. If a plugin is marked unusable, reinstall it from its original source; an unusable plugin whose kind cannot be read is listed in both `/ticks` and `/plugins`.

## The /plugins pages

`/plugins` works like `/ticks`: Installed and Available pages, `Enter` for actions (enable/disable, update) or to install, `/` to filter, and `x` twice to uninstall. Installed rows also show unsupported components and how many of the plugin's hooks need confirming. A failed marketplace has its own error row, summarized in at most three lines; other sources and Installed remain usable.

The third page of `/plugins`, **Marketplaces**, does what `tickloop plugin marketplace` does. Each row shows the name, source, when it was last updated and whether it is built in; a marketplace that cannot be read shows its error under its row. `a` opens a one-line field that takes the same sources as `marketplace add`; `Enter` adds, `Ctrl+G` opens an editor and `Esc` cancels. `Enter` on a row reads that marketplace again, like `marketplace update NAME`. `x` twice on the same row within 3 seconds removes it and keeps the plugins installed from it; `tickloop` cannot be removed, so its row has no `x remove` hint and ignores `x`. The default `claude-plugins-official` row can be removed and the notice explains how to add it back. Adding, updating and removing run in the background, and the Available page follows. `/ticks` has no Marketplaces page and points to `/plugins`.

## Skills

Type `/` to discover user-invocable Skills for the current folder, including built-in, user, project and enabled plugin and Tick Skills. The Skills group shows each name, description and source; use `Ctrl+N` / `Ctrl+P` to select, then `Enter` or `Tab` to complete. Completion refreshes when Skills or plugins change. Invoke `/plugin:skill [ARGUMENTS]`, or `/skill [ARGUMENTS]` when that short name is unique. Ambiguous short names list the full names to use; exact names win, and built-in commands always take precedence. Each invocation displays a `Skill · name · source` row, preserved when reopening history.

Enabled plugin Skills appear as `plugin:skill` from the next model request, without restarting. Disabling hides its Skills; reinstalling or updating preserves the disabled state. Installation places files; enabled MCP servers start with the Agent or on its next request. Plugin Skill and MCP tool calls still require the usual approval. Skills, Commands exposed as Skills, Tick programs, MCP servers, hooks and plugin user configuration are supported; for hooks, installation and `tickloop plugin list` show how many need confirming and warn about the events, types and fields Tickloop skips (see [Hooks](hooks.md)); other components are listed as unsupported. Uninstalling a plugin also removes the trust records of its hooks. npm and pip plugin sources are listed but cannot be installed. Installation, updates, removal and login are user actions, not actions the model initiates.

## Commands

Plugin Markdown commands are also Skills: invoke `/plugin:command` or call `Skill` with its full name. A manifest's `commands` selects files, directories or inline content; without it, `commands/` is scanned recursively. Subdirectories add namespace segments, and a directory's `SKILL.md` takes precedence over other Markdown files there. Commands take precedence over Skills with the same full name. `argument-hint` appears in completion; `user-invocable: false` hides user invocation, and `disable-model-invocation: true` rejects model invocation. Commands without an explicit description or `when_to_use` are omitted from the model catalog but remain callable by their exact name.

Commands substitute `$ARGUMENTS`, zero-based `$0`, `$1`, `$ARGUMENTS[0]`, and names declared by `arguments`; quoted arguments stay together. Missing positional arguments stay as placeholders. With no substituted placeholder, nonempty arguments are appended. Shell blocks written as ! followed by backticks, or fenced with ` ```! `, run only on invocation, through ordinary Bash preparation and approval. Their output enters the instructions only after successful completion; a failure or unknown outcome stops preparation and is never retried automatically. Installation, discovery and completion execute no command blocks. User invocations pass `SessionStart` and `UserPromptSubmit` before guidance is activated or Shell blocks run. After a restart, only guidance proven ready for that invocation without Shell can be reused; a missing preparation record requires invoking the command again explicitly.

`allowed-tools` warns and is ignored, granting no permissions. `model` (including `inherit`) is ignored with a warning, keeping the Chat's current model. `@file` remains text: the Agent reads referenced files with `Read` under its usual permissions. Unsupported command fields, including `name` aliases, `context`, `agent`, `background`, `effort`, `shell` and frontmatter `hooks`, warn and are ignored; invocation flags must be YAML booleans. Only enabled plugins contribute Commands; user and project `.claude/commands` directories are not read.

## Plugin user configuration

Plugins may declare `userConfig` fields (string, number, boolean, directory or file). Installation and enablement ask for missing values in a terminal; scripts receive a `tickloop plugin configure NAME` hint. Use that command or the Configure action in `/plugins` to change values later. Sensitive input is hidden or masked, does not open an external editor with `Ctrl+G`, and is saved in `credentials/NAME/user-config.json` with owner-only access; other values live in `plugins.NAME.options` in `settings.json`. Uninstalling preserves both. MCP configuration receives the values; Skills receive public values and a placeholder for sensitive values. Changes take effect on the next request.

User-configured servers take priority over plugin servers with the same command and arguments or URL. Between plugins, the first plugin by name wins; skipped duplicates produce a warning. Plugin root and data placeholders (`${TICKLOOP_PLUGIN_ROOT}`, `${TICKLOOP_PLUGIN_DATA}` and their `CLAUDE_` aliases), plus startup environment variables `${VAR}` and `${VAR:-default}`, are expanded when connecting. Services referencing missing `${user_config.*}` values show "needs configuration" and do not start. HTTP authentication is discovered on connection; validation alone cannot tell whether login is required.

Existing Tick packages migrate automatically on first use, keeping saved login. Broken packages are reported for reinstallation; the old directory is preserved.
