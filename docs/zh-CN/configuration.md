# 配置

[Tickloop](../../README.zh-CN.md) · [English](../configuration.md) | 简体中文

本手册里出现的 `settings.json` 键。设置、登录与历史保存在 `~/.tickloop`。完整行为见链接；某页写明仓库文件不能设置的键，仓库文件就不能设置。

| 键 | 作用 |
| --- | --- |
| [`autoCompactWindow`](interface.md#上下文) | token 数。小于模型窗口时，压缩触发点改按它计算。此后新建或重新打开的对话生效。 |
| [`cleanupPeriodDays`](interface.md#agents-总览) | 已结束的 Agent 列出这么多天（默认 30）。不会自动重启。 |
| [`defaultMode`](permissions.md#执行模式) | 新 Chat 的初始模式。取值与 `--mode` 相同。不可用的值只警告，新 Chat 以 Auto 启动。 |
| [`useAutoModeDuringPlan`](permissions.md#执行模式) | 设为 `false` 时，从 Auto 进入 Plan 后，原本交给 Auto 审查的操作改为询问。 |
| [`permissions.additionalDirectories`](permissions.md#附加目录) | 新 Agent 默认包含的目录。同一字段放在 `projects["/项目绝对路径"]` 下时只对该工作目录生效，并与全局列表合并。 |
| [`sandbox`](sandbox.md) | macOS 上 Bash、后台 Shell 和 `!` Shell 的沙箱。只读你自己的设置。目录标量在 `projects["<规范化 Folder 路径>"].sandbox`，覆盖全局 `sandbox`；列表字段两层合并。 |
| [`sandbox.enabled`](sandbox.md#字段) | 默认 `false`。让 Bash、后台 Shell 和 `!` Shell 在沙箱中运行。 |
| [`sandbox.failIfUnavailable`](sandbox.md#字段) | 默认 `false`。沙箱不可用时：`false` 警告后按无隔离运行，`true` 拒绝执行。 |
| [`sandbox.autoAllowBashIfSandboxed`](sandbox.md#字段) | 默认 `true`。Ask 与 Auto 下，沙箱中的 Bash 和后台 Shell 直接执行，不问人也不送 Auto 审查。 |
| [`sandbox.allowUnsandboxedCommands`](sandbox.md#字段) | 默认 `true`。允许模型通过 `dangerouslyDisableSandbox` 申请越出。`false` 忽略该请求；`excludedCommands` 仍生效。 |
| [`sandbox.excludedCommands`](sandbox.md#字段) | 默认 `[]`。总在沙箱外运行并走正常审批的命令。 |
| [`sandbox.filesystem.allowWrite`](sandbox.md#字段) | 默认 `[]`。追加可写路径。 |
| [`sandbox.filesystem.allowGitConfig`](sandbox.md#字段) | 默认 `false`。放开 `.git/config` 写入。Git hooks 与其他保护仍生效。 |
| [`sandbox.filesystem.denyWrite`](sandbox.md#字段) | 默认 `[]`。保持只读的路径，优先于 `allowWrite`。 |
| [`sandbox.network.allowedDomains`](sandbox.md#字段) | 默认 `[]`。经本机代理可以访问的目标。 |
| [`mcpServers`](mcp.md#mcp-服务) | 你自己的 MCP 服务，所有目录生效。`projects[<目录>].mcpServers` 覆盖同名全局服务。 |
| [`mcpServers.<name>.type`](mcp.md#mcp-服务) | `stdio` 或 `http`。 |
| [`mcpServers.<name>.command`、`args`](mcp.md#mcp-服务) | 本地服务的命令和参数。 |
| [`mcpServers.<name>.url`](mcp.md#mcp-服务) | 远程 URL。须为 HTTPS，仅本机回环地址允许 HTTP。 |
| [`mcpServers.<name>.exposure`](mcp.md#mcp-服务) | `direct`、`search` 或 `hidden`。省略时按 `search`。 |
| [`mcpServers.<name>.timeout`](mcp.md#mcp-服务) | 单次工具调用上限，单位毫秒（默认 300000）。小于 1000 的值忽略。 |
| [`oauth.clientId`](mcp.md#管理服务与登录) | 远程条目的 OAuth client id。 |
| [`oauth.callbackPort`](mcp.md#管理服务与登录) | 远程条目的 OAuth 回调端口。 |
| [`authServerMetadataUrl`](mcp.md#管理服务与登录) | OAuth 元数据 URL。也可写在 `oauth` 内。 |
| [`disabledMcpServers`](mcp.md#管理服务与登录) | 位于 `projects[<目录>]`。在该目录停用的服务，包括插件服务。 |
| [`pluginMcpServerExposure`](mcp.md#管理服务与登录) | 全局对象，按完整服务名记录插件服务的暴露方式。覆盖插件自带的 `exposure`，插件更新后仍然有效。 |
| [`toolExposure`](mcp.md#tool-search) | 按工具原名覆盖。精确名优先，其次是第一个匹配的 `*` 通配。 |
| [`plugins.<name>.options`](plugins.md#插件用户配置) | 插件 `userConfig` 的非敏感值。敏感值存在凭据旁，不在这里。 |
| [`hooks`](hooks.md) | 你的设置里的 command Hook，对所有目录生效，或写在 `projects["/absolute/project/path"]` 下。 |
| [`hookTrust`](hooks.md#信任与-hooks) | Hook 的信任记录。仓库的记在该目录的 `projects` 条目里。 |
