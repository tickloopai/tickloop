# MCP

[Tickloop](../../README.zh-CN.md) · [English](../mcp.md) | 简体中文

插件如何声明 MCP 服务，与插件安装的其余内容一起写在 [插件](plugins.md)。本页说明这些服务，以及你自己添加的服务，如何配置、搜索和登录。

## MCP 服务

已启用插件可在 `.mcp.json` 中声明 MCP 服务（服务映射或带 `mcpServers` 外层），也可在清单的 `mcpServers` 中使用对象、相对路径或数组。支持 stdio 与 streamable HTTP，工具名为 `mcp__plugin_<插件>_<服务>__<工具>`；名称各部分的连续下划线合并为一个；过长或碰撞的工具名补哈希并保留服务／工具分隔符，规范化后同名的插件服务会跳过并警告。`validate`、安装与启用会列出命令、地址及缺少的配置。启用即同意从下一次 Agent 请求起以用户身份运行服务；禁用或卸载后从下一次请求起停止，工具调用仍按用户权限审批。

你自己的 MCP 服务写在 `settings.json` 的 `mcpServers` 中（所有目录生效），或 `projects[<目录>].mcpServers` 中（只对该目录生效）；`tickloop-artifacts` 和以 `plugin_` 开头的名字保留，用户服务名包含 `__` 时拒绝并提示。`tickloop mcp add <名> <URL> [--header 'Name: value']...` 添加远程服务，`tickloop mcp add <名> [--env NAME=value]... -- <命令> [参数...]` 添加本地服务；默认写入全局设置，加 `--dir` 写入当前目录。`--exposure direct|search|hidden` 设定该服务的工具怎样到达模型；不加时条目不含 `exposure`，按 `search` 处理。`tickloop mcp remove <名> [--dir]` 删除服务。`tickloop mcp list` 按来源列出在当前目录启动的 Agent 会有的全部服务，及其连接方式、暴露方式、是否在此停用、是否已登录，不启动任何服务。

例如，将下面内容合入配置目录中的 `settings.json`，把命令和 URL 换成你的服务：

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

目录配置覆盖同名全局服务。用户服务名可用字母、数字、`_` 和 `-`；仓库中的 `.mcp.json` 不会自动加载。远程地址要求 HTTPS，仅本机回环地址允许 HTTP；旧式 SSE 与 WebSocket 列为暂不支持。可选 `timeout` 是单次工具调用上限，单位毫秒，默认 300000，小于 1000 的值忽略。每个根 Agent 分别运行服务，子 Agent 共用；MCP 服务不是 Tick，不出现在 Monitor 中。

没有权限规则时，第三方工具在 Ask 模式下逐次询问；在 Auto 模式下由 Auto 审查判断，不放行时才询问你。服务自称只读不改变这两种处理。确认时可只允许一次、在当前目录总是允许该工具或整个服务，或拒绝。Plan 模式下模型被要求只调用只读的 MCP 工具，调用按进入 Plan 之前的模式处理：之前是 Auto 则交给 Auto 审查（`useAutoModeDuringPlan` 关闭时改为询问），之前是 Full Access 则不再询问，其他情况询问。Full Access 直接执行；显式 Ask 与 Deny 规则仍然优先。调用发出后超时、取消或断线，可能无法确定是否已执行，重试修改前应先核实结果。资源与 Artifacts 共用 `ListMcpResourcesTool` 和 `ReadMcpResourceTool`。

## Tool Search

MCP 工具默认采用 `exposure: "search"`。模型卡支持原生 Tool Search 时，模型用自身搜索找到工具后直接调用；其他模型采用 Fold：通过 `SearchTools` 搜索（一个 `query`，按空格分词、任一词命中即可；也可写完整工具名、`mcp__<服务>` 列出该服务的工具、`select:A,B` 按名选取，或用 `+词` 要求必须命中）、`DescribeTools` 查看完整参数，再用 `ExecuteTools` 并发执行 1–10 个独立调用，每次调用分别审批。搜索结果也会列出仍在连接、连接失败或需要登录的服务。第一次请求为仍在连接的服务总共最多等待 2 秒，等待时状态行显示在等哪些服务；之后才连上的服务从下一次请求起可被搜到，其直接列出的工具在之后的某一轮开始时加入工具列表。远程服务断开或连接失败后，依次在 1、2、4、8、16 秒后重试，之后保持失败；本地服务启动失败或退出后不会自动重启，直到你在 `/mcp` 中重连、修改其配置或重启 Agent。服务重连或续期登录期间，最近一次列出的工具仍然可用。切换模型时自动选择可用方式。设为 `direct` 可直接列入模型工具列表，`hidden` 则不可调用。`toolExposure` 按工具原名覆盖：精确名优先，其次取按声明顺序第一个匹配的 `*` 通配模式。服务变化以新上下文消息追加，Fold 工具定义保持不变。

`ExecuteTools` 批量调用运行中 Agent 进程退出时不会重跑：载入的历史列出批量里每次调用的结局。对话中批量里的每次调用显示在它的 `ExecuteTools` 调用之下。

工具结果行显示 Auto 审查中或等待人工批准。放行后，在结果下方独立一行以弱化文字显示 Auto 审批方或“Approved by you”；直接放行的调用不显示审批标签。人工审批另保留一行说明确认来源与原因，拒绝的调用显示拒绝方。ExecuteTools 为每个嵌套调用显示状态，并汇总整批结果。重开对话后，仍保留的审批记录会恢复对应标签。

## 管理服务与登录

`/mcp` 按全局、当前目录和各插件分组显示 Agent 的服务，每行有连接方式、状态（已连接、连接中、失败、需要登录、需要配置、已停用、暂不支持）、工具数和暴露方式。在服务上按 Enter 可以查看工具、登录或重新登录、退出登录、重连、在当前目录停用或启用（插件服务同样适用，记在 `projects[<目录>].disabledMcpServers`），以及修改暴露方式。登录或重连后服务立即连接，行内先显示 `connecting`，列出工具后显示 `connected · N tools`，失败则显示 `failed`，原因写在操作菜单里。Agent 启动前，打开面板会连接远程服务以列出工具，随后关闭这些连接；本地（stdio）服务显示 `starts with the Agent`，只有选 Connect 或按 `r` 才连接。在服务上按 `e`，暴露方式按 `search` → `direct` → `hidden` → `search` 循环切换，每次立即保存。用户服务的 `exposure` 写回该条目所在的那一层（全局与当前目录同名时写到当前目录），改为 `search` 时删除该字段；插件服务的选择按完整服务名记在全局设置的 `pluginMcpServerExposure` 对象中（例如 `"pluginMcpServerExposure": {"plugin_docs_search": "direct"}`），覆盖插件自带的 `exposure`，插件更新后仍然有效。运行中的 Agent 从下一次请求起按新暴露方式提供工具，一轮进行中也是如此。用 `Ctrl+N`/`Ctrl+P` 或上下键选择，`Enter` 打开操作，`e` 切换暴露方式，`r` 刷新，`Esc` 返回。添加和删除服务使用 `tickloop mcp add` 和 `remove`，面板只管理已有条目；`toolExposure` 在 `settings.json` 中编辑。登录页显示链接，本机可以打开浏览器；远程使用时在自己的电脑上打开链接，把完整回调地址粘贴回面板；`Esc` 或 `Ctrl+C` 取消。`/plugins` 的已安装行显示插件带几个 MCP 服务以及是否需要登录。

远程 MCP 服务需要 OAuth 时，运行 `tickloop mcp login <服务>`，在浏览器打开终端打印的链接。Tickloop 可以自动接收本机回调；通过 SSH 使用时，也可以把浏览器跳转后的完整地址粘贴回终端。Ctrl+C 取消，不覆盖已有登录；终端回调输入支持 Ctrl+G 打开外部编辑器。插件服务支持 `plugin:<插件>:<服务>` 和 `plugin_<插件>_<服务>` 两种写法。

在 `/mcp` 登录后立即连接该服务：Agent 运行时由 Agent 连接，启动前由面板连接；其他方式登录从下一次 Agent 请求起生效。令牌自动续期。`tickloop mcp logout <服务>` 在服务器支持时撤销令牌，并删除本地令牌、保留客户端注册。凭据只保存在本机私有的 `credentials/_mcp/` 或 `credentials/<插件>/mcp/` 目录中；服务地址变化后不使用旧令牌。配置固定 `Authorization` header 的服务不走 OAuth。远程服务支持 `oauth.clientId`、`oauth.callbackPort` 和 `authServerMetadataUrl`（也接受放在 `oauth` 内）。

服务配置中的 `${VAR}`、`${VAR:-默认值}` 使用 Agent 的启动环境；缺变量且无默认值时，该服务不启动。`/mcp` 与 CLI 登录使用各自进程的环境。如果 URL 变量与 Agent 启动时的值不同，登录会保存到另一个地址，Agent 无法使用；请让两个终端使用相同的值。模型会请用户添加、配置或登录服务，不会自行执行这些操作。
