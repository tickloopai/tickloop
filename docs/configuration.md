# Configuration

[Tickloop](../README.md) · English | [简体中文](zh-CN/configuration.md)

`settings.json` keys named in this manual. Settings, logins and history are kept in `~/.tickloop`. Follow the link for the full behavior; repository files cannot set the keys whose pages say so.

| Key | Effect |
| --- | --- |
| [`autoCompactWindow`](interface.md#context) | Token count. When it is smaller than the model's window, the compaction point is measured in it instead. Conversations started or reopened afterwards use it. |
| [`cleanupPeriodDays`](interface.md#agents-overview) | Ended Agents stay listed until this many days pass (30 by default). Nothing restarts them. |
| [`defaultMode`](permissions.md#execution-modes) | Mode new Chats start in. Same names as `--mode`. An unusable value warns, and new Chats start in Auto. |
| [`useAutoModeDuringPlan`](permissions.md#execution-modes) | Set to `false` to be asked, after Auto, about what Plan would otherwise leave to Auto review. |
| [`permissions.additionalDirectories`](permissions.md#additional-directories) | Directories new Agents include by default. The same field under `projects["/absolute/project/path"]` applies only to that working directory and adds to the global list. |
| [`sandbox`](sandbox.md) | macOS sandbox for Bash, background Shells and `!` Shells. Read only from your own settings. Folder scalars live under `projects["<canonical Folder>"].sandbox` and override the global `sandbox`; the list fields merge. |
| [`sandbox.enabled`](sandbox.md#fields) | Default `false`. Run Bash, background Shells and `!` Shells in the sandbox. |
| [`sandbox.failIfUnavailable`](sandbox.md#fields) | Default `false`. When the sandbox is unavailable: `false` warns and runs without isolation, `true` refuses to run. |
| [`sandbox.autoAllowBashIfSandboxed`](sandbox.md#fields) | Default `true`. In Ask and Auto, sandboxed Bash and background Shell commands run without asking or Auto review. |
| [`sandbox.allowUnsandboxedCommands`](sandbox.md#fields) | Default `true`. Allow model requests through `dangerouslyDisableSandbox`. `false` ignores those requests; `excludedCommands` still applies. |
| [`sandbox.excludedCommands`](sandbox.md#fields) | Default `[]`. Commands that always run outside the sandbox, with ordinary approval. |
| [`sandbox.filesystem.allowWrite`](sandbox.md#fields) | Default `[]`. More writable paths. |
| [`sandbox.filesystem.allowGitConfig`](sandbox.md#fields) | Default `false`. Permit `.git/config` writes. Git hooks and other protections remain. |
| [`sandbox.filesystem.denyWrite`](sandbox.md#fields) | Default `[]`. Paths kept read-only. Wins over `allowWrite`. |
| [`sandbox.network.allowedDomains`](sandbox.md#fields) | Default `[]`. Destinations reachable through the local proxy. |
| [`mcpServers`](mcp.md#mcp-servers) | Your MCP servers, for every directory. `projects[<directory>].mcpServers` overrides a global server of the same name. |
| [`mcpServers.<name>.type`](mcp.md#mcp-servers) | `stdio` or `http`. |
| [`mcpServers.<name>.command`, `args`](mcp.md#mcp-servers) | Local server command and arguments. |
| [`mcpServers.<name>.url`](mcp.md#mcp-servers) | Remote URL. HTTPS, or HTTP on loopback only. |
| [`mcpServers.<name>.exposure`](mcp.md#mcp-servers) | `direct`, `search` or `hidden`. Omitted means `search`. |
| [`mcpServers.<name>.timeout`](mcp.md#mcp-servers) | Tool-call limit in milliseconds (default 300000). Values below 1000 are ignored. |
| [`oauth.clientId`](mcp.md#manage-servers-and-log-in) | OAuth client id on a remote entry. |
| [`oauth.callbackPort`](mcp.md#manage-servers-and-log-in) | OAuth callback port on a remote entry. |
| [`authServerMetadataUrl`](mcp.md#manage-servers-and-log-in) | OAuth metadata URL. Also accepted inside `oauth`. |
| [`disabledMcpServers`](mcp.md#manage-servers-and-log-in) | Under `projects[<directory>]`. Servers turned off in that directory, plugin servers included. |
| [`pluginMcpServerExposure`](mcp.md#manage-servers-and-log-in) | Global object of plugin server exposure by full name. Overrides the plugin's own `exposure` and outlives plugin updates. |
| [`toolExposure`](mcp.md#tool-search) | Overrides individual original tool names. Exact names win, then the first matching `*` pattern. |
| [`plugins.<name>.options`](plugins.md#plugin-user-configuration) | Non-sensitive plugin `userConfig` values. Sensitive values are stored beside credentials, not here. |
| [`hooks`](hooks.md) | Command hooks in your settings, for every folder or under `projects["/absolute/project/path"]`. |
| [`hookTrust`](hooks.md#trust-and-hooks) | Trust records for hooks. A repository's records live in that folder's `projects` entry. |
