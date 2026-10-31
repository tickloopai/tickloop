# MCP

[Tickloop](../README.md) · English | [简体中文](zh-CN/mcp.md)

Plugins that declare MCP servers are covered with the rest of plugin install in [Plugins](plugins.md). This page is how those servers, and servers you add yourself, are configured, searched and logged in.

## MCP servers

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

## Tool Search

MCP tools default to `exposure: "search"`. Models whose cards support native Tool Search find tools with their own search and then call them directly. Other models use Fold: `SearchTools` finds tools from one `query` of space-separated words (any word matches; also an exact tool name, `mcp__<server>` for a server's tools, `select:A,B`, or `+word` to require a word), `DescribeTools` supplies their full parameters, and `ExecuteTools` runs 1–10 independent calls with individual approval. Searches also name servers that are still connecting, failed or need login. The first request waits at most 2 seconds in total for servers that are still connecting, and the status line names them meanwhile; a server that connects later can be found from the next request, and its directly listed tools join the tool list at a later turn. A remote server that disconnects or fails to connect tries again after 1, 2, 4, 8 and 16 seconds and then stays failed; a local server that fails or exits is not started again until you reconnect it in `/mcp`, change its configuration or restart the Agent. While a server reconnects or renews its login, its last listed tools stay available. Switching models selects the available form automatically. Use `direct` to list tools in every model request, or `hidden` to make them unavailable. `toolExposure` overrides individual original tool names; exact names win, then the first matching `*` pattern. Service updates append context without changing Fold tool definitions.

If the Agent's process exits while an `ExecuteTools` batch runs, the batch is not run again: the loaded history lists what each of its calls did. In the conversation, each call a batch makes shows under its `ExecuteTools` call.

Tool result rows show an Auto review in progress or a wait for your approval. Once allowed, a separate muted line below the result names the Auto reviewer or says “Approved by you”. Directly allowed calls have no approval label. Human approvals keep an extra line explaining why you were asked; denied calls show who denied them. ExecuteTools labels each nested call and summarizes the batch. Reopened conversations restore labels for the approval records still retained.

## Manage servers and log in

`/mcp` shows the Agent's servers grouped as global, this directory and each plugin, with transport, state (connected, connecting, failed, needs login, needs configuration, disabled, not supported yet), tool count and exposure. Enter on a server shows its tools, logs in again or out, reconnects it, turns it off or on in this directory, plugin servers included (`projects[<directory>].disabledMcpServers`), or changes its exposure. After a login or Reconnect the server connects at once and its row shows `connecting` until it has listed its tools (`connected · N tools`) or failed, with the cause under its actions. Before the Agent starts, opening the panel connects remote servers to list their tools and closes those connections again; local (stdio) servers show `starts with the Agent` and connect only through Connect or `r`. `e` on a server moves its exposure from `search` to `direct` to `hidden` and back to `search`, saving each step at once. A user server's `exposure` is written in the layer that configures it, the directory's when both layers have the name, and `search` removes the field; a plugin server's is kept in the global `pluginMcpServerExposure` object by its full name (for example `"pluginMcpServerExposure": {"plugin_docs_search": "direct"}`), which overrides the plugin's own `exposure` and outlives plugin updates. A running Agent offers the new exposure from its next request, even in the middle of a turn. Use `Ctrl+N`/`Ctrl+P` or up/down to select, `Enter` for actions, `e` for exposure, `r` to refresh and `Esc` to go back. Add and remove services with `tickloop mcp add` and `remove`; the panel manages existing entries, and `toolExposure` is edited in `settings.json`. A login in `/mcp` shows a link and can open a browser locally; on a remote host, open it on your own computer and paste the full callback address back into the panel. `Esc` or `Ctrl+C` cancels. `/plugins` shows how many MCP servers an installed plugin carries and whether one needs a login.

For a remote MCP server that needs OAuth, run `tickloop mcp login <server>`. Open the printed link in your browser; Tickloop receives the callback locally, or you can paste the full callback address into the terminal when using SSH. Ctrl+C cancels without replacing your saved login. In terminal callback input, Ctrl+G opens your external editor. Plugin servers accept both `plugin:<plugin>:<server>` and `plugin_<plugin>_<server>`.

A login in `/mcp` connects the server right away, through the running Agent or, before it starts, for the panel; otherwise a login becomes available from the next Agent request. Tokens renew automatically. `tickloop mcp logout <server>` revokes the token where supported and deletes it locally, keeping the client registration. Credentials are private local files under `credentials/_mcp/` or `credentials/<plugin>/mcp/`; changing a server address prevents reuse of its old token. A fixed `Authorization` header bypasses OAuth. Remote entries support `oauth.clientId`, `oauth.callbackPort`, and `authServerMetadataUrl` (also accepted inside `oauth`).

Service configuration expands `${VAR}` and `${VAR:-default}` from the Agent's startup environment; a missing variable without a default prevents that server from starting. Login in `/mcp` or the CLI uses that process's environment. If a URL variable differs from the Agent's environment, login is saved for a different address and the Agent cannot use it: use matching values in both terminals. The model asks you to add, configure or log in to servers; it does not do those steps itself.
