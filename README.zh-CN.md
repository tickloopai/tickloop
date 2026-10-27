# Tickloop

[官网](https://tickloop.ai) · [English](README.md) | 简体中文

Tickloop 是在终端里工作的 AI Agent。用日常语言告诉它要做什么：它会读懂你的项目、修改代码、运行测试，整理资料、撰写文档、制作页面和演示，并一直推进到完成。

它的独特之处是 **Tick**：你和 Agent 一起看着的运行中程序，比如日志查看器、看板或一条长时间运行的命令。Agent 可以读取 Tick、操作它，并在其中有变化时被唤醒。

## 能做什么

- **交给它真正的工作。** Agent 修改文件、运行命令并检查结果。默认是 Auto 模式：常规操作由审查模型放行，有风险的操作会问你。
- **帮你盯着变化。** 把程序作为 Tick 打开并订阅，它一变化 Agent 就会响应。对话旁的 Monitor 列出你的 Tick、后台命令和定时任务。
- **定时重复。** `/loop 5m 检查部署` 每五分钟执行一次；`/goal` 设一个目标，Agent 持续推进直到达成。
- **选你喜欢的模型。** 内置 Claude、OpenAI（API 或 ChatGPT 订阅）、GitHub Copilot、Gemini、DeepSeek、Kimi、MiniMax、GLM、xAI 和阿里云，随时用 `/model` 切换。
- **分享成果。** 把文档、网页和演示发布到 Tickloop Artifacts，并收到他人的反馈。
- **离开再回来。** 关闭终端不会停止 Agent，`tickloop -c` 接着最近的对话继续。

## 安装

macOS：

```sh
brew install tickloopai/tap/tickloop
```

macOS 或 Linux：

```sh
curl -fsSL https://tickloop.ai/install.sh | sh
```

已安装 Rust 时也可以用 `cargo install --locked tickloop`。三种方式都会安装 `tickloop` 命令和短命令 `tl`，并在后台自动保持最新。要立即更新，运行 `tickloop update`。

## 开始使用

1. 在项目目录运行 `tickloop`。
2. 首次启动时选择模型服务商，输入 API Key 或登录，再选择模型。
3. 描述你要完成的事。

想看看 Tick 怎么用？对 Agent 说：

> 以 `protocol: true` 打开 `python3 tick/examples/counter.py`，并订阅它。

然后在 Monitor 中对 Counter 按 Enter，输入 `+` 再按 Enter：Agent 会收到新的计数并作出回应。

## 日常使用

| 命令 | 作用 |
| --- | --- |
| `/help` | 列出全部命令 |
| `/model`、`/provider` | 切换模型，或登录其他服务商 |
| `/plan` | 先做计划，确认后才开始改动 |
| `/goal`、`/loop` | 持续推进一个目标；重复执行一条指令 |
| `/stop` | 停下 Agent 正在做的一切 |
| `/rewind`、`/diff` | 撤回到之前的某条消息；查看文件改动 |
| `/resume`、`/clear` | 重新打开以前的对话；开始新对话 |
| `/permissions`、`/settings` | 设置 Agent 可以自行做多少事；其他偏好 |
| `/ticks`、`/plugins` | 安装并登录 Tick；管理其他插件 |
| `/mcp` | 查看 MCP 服务，管理登录、暴露方式及当前目录的启用状态 |
| `/context` | 按类别查看上下文占用，以及工具搜索能找到多少 MCP 工具；`/context all` 列出每一项 |
| `/artifacts` | 登录 Artifacts，把 Artifact 加入对话 |
| `/exit` | 离开界面（Agent 继续运行） |

按键：

- `enter` 发送消息。Agent 工作时消息会等到下一步再处理；`ctrl+enter` 打断并立即发送。
- `esc` 打断 Agent。输入为空时连按两次，可撤回到之前的某条消息。
- `shift+tab` 在 Auto 与 Plan 模式之间切换。
- `ctrl+j` 换行；`ctrl+g` 用外部编辑器编辑消息。
- 列表和菜单中用 `j`／`k` 或方向键移动，`enter` 选择，`esc` 返回。
- 在 Monitor 中 `enter` 打开 Tick，`esc` 再隐藏；程序会继续运行。
- 在 Monitor 同一行三秒内按两次普通 `x`，可关闭或移除 Tick、停止 Shell、解除 Artifact 关联或删除定时任务。第一次只在按键提示行显示红色 `Press x again to …`，没有确认弹窗；超时、移动选择、按其他键或离开会取消。`Ctrl+X` 不执行这些操作；文本输入和筛选框中的 `x` 仍是文字。

对话接近模型的上下文窗口上限时，Agent 会在下一次请求前先把对话压缩成摘要。触发点是窗口减去摘要所需的输出空间（模型的输出上限，最多 20,000 tokens）再减去 13,000 tokens 的缓冲，但不低于窗口的一半；触发点之后的空间就是 `/context` 中显示的 Autocompact buffer。距触发点不到 20,000 tokens 时，状态行的 `Context N% used` 改为警示色，对话中也会提示一次；随时可以用 `/compact` 手动压缩。想更早压缩，可在 `settings.json` 中把 `autoCompactWindow` 设为 token 数，例如 `"autoCompactWindow": 400000`；它小于模型窗口时，触发点改按它计算。此后新建或重新打开的对话生效。

Herdr 内的 Monitor 按配置根分别注册浮窗插件。默认 `~/.tickloop` 继续使用 `tickloop.monitor` 和 `~/.tickloop/herdr-popup-plugin`；其他根（包括 `make dev` 的 `~/.tickloop-dev`）使用 `tickloop.monitor.<root-key>`，目录仍为 `<配置根>/herdr-popup-plugin`，显示名包含根路径。稳定的 root key 与 daemon、Tickhost 共用，两套插件可在同一 Herdr 会话共存。每个根首次使用时接受 Monitor 的注册提示，或执行 `tickloop monitor popup install --herdr-socket SOCKET`；`remove` 只移除当前根的插件。默认根已有注册无需重做。

## Tick

Tick 是 Agent 可以打开、盯着并操作的实时程序，比如行情源或文档阅读器。Tick 和插件一样安装，共用 marketplace 和存储，但单独管理：用 `/ticks` 查找、安装和登录 Tick，或在命令行中操作：

```sh
tickloop tick list --available
tickloop tick install NAME                # 或 NAME@MARKETPLACE
tickloop tick list
tickloop tick login NAME                  # 数据源需要登录时使用
tickloop tick logout NAME
tickloop tick update NAME                 # 省略 NAME 更新所有 Tick
tickloop tick disable NAME
tickloop tick enable NAME
tickloop tick uninstall NAME              # 删除其数据，保留登录
tickloop tick uninstall NAME --keep-data  # 同时保留数据
tickloop tick install --path ./my-tick    # 本地 Tick 目录或 .tar.gz 压缩包
```

官方 Tick 来自内置的 `tickloop` marketplace，源码在 [tickloopai/ticks](https://github.com/tickloopai/ticks)。带 Tick 程序的插件就是 Tick，即使它也带 Skill：装好后只出现在 `/ticks` 和 `tickloop tick` 中，不出现在 `/plugins`。安装前就能看出种类时（Tick marketplace 的包或本地目录），`tickloop plugin` 拒绝 Tick 并给出应改用的 `tickloop tick` 命令，反之亦然。

在 `/ticks` 中，用 `Tab` 或左右键切换 **Installed**（已安装）和 **Available**（可安装），`j`/`k` 或 `Ctrl+N`/`Ctrl+P` 选择。已安装页按 `Enter` 打开 Tick 的操作：登录/退出登录、更新、启用/禁用；每行显示 marketplace、启用状态、版本和登录状态。登录在面板内完成：OAuth 的 Tick 显示链接和验证码，在任意设备的浏览器中确认；密钥以遮掩方式输入。可安装页按 `Enter` 安装或更新所选 Tick，按 `/` 开始过滤名字、说明和 marketplace，不区分大小写；过滤时 `j`/`k` 是文字，`Ctrl+N`/`Ctrl+P` 选择，`Ctrl+G` 打开外部编辑器。`Esc` 退出过滤并保留查询，再按一次关闭面板。

在已安装页同一行三秒内按两次 `x` 卸载：删除该 Tick 的数据，保留登录，不停止运行中的 Tick。移动选择、按其他键、离开或超时都会取消确认。登录保留在配置目录的 `credentials/NAME/`；要删除保存的登录，先退出登录再卸载。

装好 Tick（需要登录的先登录）后，让 Agent 按名字打开它，例如"打开 NAME 这个 Tick 并盯着它"。安装不运行任何程序：Tick 程序只在按你的权限设置打开时以你的身份运行。禁用的 Tick 不能按名字打开。安装、更新、卸载和登录由你发起，Agent 只会请你去做。

程序实现 [Tick 协议](tick/protocol/SPECIFICATION.md) 就能成为扩展 Tick；可从 [Counter 示例](tick/examples/counter.py) 开始。

## 插件

插件为 Agent 增加 Skill 和 MCP 服务。用 `/plugins` 浏览不带 Tick 程序的已安装和可安装插件，或在命令行中操作：

```sh
tickloop plugin list --available
tickloop plugin install NAME@MARKETPLACE
tickloop plugin list
tickloop plugin update NAME                 # 省略 NAME 更新所有 marketplace 插件
tickloop plugin disable NAME
tickloop plugin enable NAME
tickloop plugin uninstall NAME              # 删除插件数据
tickloop plugin uninstall NAME --keep-data  # 同时保留插件数据
```

`tickloop` 是不能删除的内置 marketplace；`claude-plugins-official` 默认登记，首次使用时才下载，可以删除且重启后不会自动恢复。用 `tickloop plugin marketplace add anthropics/claude-plugins-official` 加回。两个名字仍保留给官方来源。来源明确时可只写 `NAME`；多个 marketplace 提供同名插件时，命令列出候选，已有安装则优先使用其来源。要把已安装的同名插件换到另一个 marketplace，先卸载。Git marketplace 中的插件若装好后才看出带 Tick 程序，由 `tickloop plugin` 安装，之后按 Tick 报告和管理。

在 `/plugins` 的 **Marketplaces** 页或命令行中登记其他来源，`/ticks` 和 `/plugins` 都会读取：

```sh
tickloop plugin marketplace add OWNER/REPO
tickloop plugin marketplace list
tickloop plugin marketplace update MARKETPLACE  # 省略名称刷新全部
tickloop plugin marketplace remove MARKETPLACE
```

`add` 也接受 `OWNER/REPO#REF`、Git 地址（HTTPS、SSH 或 file）、本地目录或 HTTPS 上的 marketplace JSON。名称取自 marketplace 文件。删除 marketplace 不会卸载已安装的插件。浏览使用上次取得的 Git 目录，用 `marketplace update` 刷新；`tickloop` 目录每次实时读取。插件 `update` 会刷新来源并检查 Git 提交，所以显示版本不变时也能更新内容。全部更新会跳过来自已删除 marketplace 的插件。

本地插件目录或 `.tar.gz` 压缩包：

```sh
tickloop plugin validate ./my-plugin
tickloop plugin install --path ./my-plugin
```

本地路径安装的插件只能重新运行 `install --path` 更新，同版本也会重装；`update` 和面板不更新它们。`validate` 报告清单、组件和路径警告；安装还会检查文件树。安装失败保留原安装。插件标为不可用时，从原来源重装；看不出种类的不可用插件同时列在 `/ticks` 和 `/plugins` 中。

`/plugins` 的用法与 `/ticks` 相同：已安装和可安装两页，`Enter` 打开操作（启用/禁用、更新）或安装，`/` 过滤，同一行按两次 `x` 卸载。已安装行还显示暂不支持的组件。取不到的 marketplace 各自显示最多三行的错误摘要，其他来源和已安装页仍可用。

`/plugins` 的第三页 **Marketplaces** 完成 `tickloop plugin marketplace` 能做的事。每行显示名字、来源、上次更新时间和是否内置；读取失败的 marketplace 在该行下显示错误。按 `a` 打开单行输入，接受与 `marketplace add` 相同的来源；`Enter` 添加，`Ctrl+G` 打开外部编辑器，`Esc` 取消。在某行按 `Enter` 重新读取该 marketplace，与 `marketplace update NAME` 相同。在同一行 3 秒内按两次 `x` 删除，已安装的插件保留；`tickloop` 行不显示 `x remove`，按 `x` 无操作也无提示；默认的 `claude-plugins-official` 可以删除，结果提示说明如何加回。添加、刷新和删除在后台进行，可安装页随之更新。`/ticks` 没有这一页，提示改到 `/plugins` 管理。

输入 `/` 可发现当前工作目录可由用户调用的 Skill，包括内置、用户、项目以及已启用插件和 Tick 的 Skill。Skills 分组显示名称、简短说明和来源；用 `Ctrl+N` / `Ctrl+P` 选择，再按 `Enter` 或 `Tab` 补全。Skill 文件或插件变化后，候选会自动刷新。用 `/plugin:skill [参数]` 调用；短名唯一时也可用 `/skill [参数]`。短名重名时会列出可用的完整名，完整名优先，内置命令始终优先。每次调用显示 `Skill · 名称 · 来源` 行，重开历史仍可见。

已启用插件的 Skill 以 `plugin:skill` 命名，从下一次模型请求起可用，无需重启。禁用会隐藏 Skill；重装或更新后仍保持禁用。安装放置文件；已启用的 MCP 服务随 Agent 启动，或从下一次请求起运行。插件 Skill 和 MCP 工具的调用照常审批。本版本支持 Skill、Tick 程序、MCP 服务和插件用户配置，其余组件列为暂不支持；npm、pip 插件来源可以列出但不能安装。安装、更新、卸载和登录由用户发起，模型不会自行执行。

### MCP 服务

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

没有权限规则时，第三方工具在 Ask 模式下逐次询问；在 Auto 模式下由 Auto 审查判断，不放行时才询问你。服务自称只读不改变这两种处理。确认时可只允许一次、在当前目录总是允许该工具或整个服务，或拒绝。Plan 模式下模型被要求只调用只读的 MCP 工具，调用按进入 Plan 之前的模式处理：之前是 Auto 则交给 Auto 审查，之前是 Full Access 则直接执行，其他情况询问。Full Access 直接执行；显式 Ask 与 Deny 规则仍然优先。调用发出后超时、取消或断线，可能无法确定是否已执行，重试修改前应先核实结果。资源与 Artifacts 共用 `ListMcpResourcesTool` 和 `ReadMcpResourceTool`。

### Tool Search

MCP 工具默认采用 `exposure: "search"`。模型卡支持原生 Tool Search 时，模型用自身搜索找到工具后直接调用；其他模型采用 Fold：通过 `SearchTools` 搜索（一个 `query`，按空格分词、任一词命中即可；也可写完整工具名、`mcp__<服务>` 列出该服务的工具、`select:A,B` 按名选取，或用 `+词` 要求必须命中）、`DescribeTools` 查看完整参数，再用 `ExecuteTools` 并发执行 1–10 个独立调用，每次调用分别审批。搜索结果也会列出仍在连接、连接失败或需要登录的服务。第一次请求为仍在连接的服务总共最多等待 2 秒，等待时状态行显示在等哪些服务；之后才连上的服务从下一次请求起可被搜到，其直接列出的工具在之后的某一轮开始时加入工具列表。远程服务断开或连接失败后，依次在 1、2、4、8、16 秒后重试，之后保持失败；本地服务启动失败或退出后不会自动重启，直到你在 `/mcp` 中重连、修改其配置或重启 Agent。服务重连或续期登录期间，最近一次列出的工具仍然可用。切换模型时自动选择可用方式。设为 `direct` 可直接列入模型工具列表，`hidden` 则不可调用。`toolExposure` 按工具原名覆盖：精确名优先，其次取按声明顺序第一个匹配的 `*` 通配模式。服务变化以新上下文消息追加，Fold 工具定义保持不变。

工具结果行显示 Auto 审查中或等待人工批准。放行后，在结果下方独立一行以弱化文字显示 Auto 审批方或“Approved by you”；直接放行的调用不显示审批标签。人工审批另保留一行说明确认来源与原因，拒绝的调用显示拒绝方。ExecuteTools 为每个嵌套调用显示状态，并汇总整批结果。重开对话后，仍保留的审批记录会恢复对应标签。

`ExecuteTools` 批量调用运行中 Agent 进程退出时不会重跑：载入的历史列出批量里每次调用的结局。对话中批量里的每次调用显示在它的 `ExecuteTools` 调用之下。

### 管理服务与登录

`/mcp` 按全局、当前目录和各插件分组显示 Agent 的服务，每行有连接方式、状态（已连接、连接中、失败、需要登录、需要配置、已停用、暂不支持）、工具数和暴露方式。在服务上按 Enter 可以查看工具、登录或重新登录、退出登录、重连、在当前目录停用或启用（插件服务同样适用，记在 `projects[<目录>].disabledMcpServers`），以及修改暴露方式。登录或重连后服务立即连接，行内先显示 `connecting`，列出工具后显示 `connected · N tools`，失败则显示 `failed`，原因写在操作菜单里。Agent 启动前，打开面板会连接远程服务以列出工具，随后关闭这些连接；本地（stdio）服务显示 `starts with the Agent`，只有选 Connect 或按 `r` 才连接。在服务上按 `e`，暴露方式按 `search` → `direct` → `hidden` → `search` 循环切换，每次立即保存。用户服务的 `exposure` 写回该条目所在的那一层（全局与当前目录同名时写到当前目录），改为 `search` 时删除该字段；插件服务的选择按完整服务名记在全局设置的 `pluginMcpServerExposure` 对象中（例如 `"pluginMcpServerExposure": {"plugin_docs_search": "direct"}`），覆盖插件自带的 `exposure`，插件更新后仍然有效。运行中的 Agent 从下一次请求起按新暴露方式提供工具，一轮进行中也是如此。用 `j`/`k` 或 `Ctrl+N`/`Ctrl+P` 选择，`Enter` 打开操作，`e` 切换暴露方式，`r` 刷新，`Esc` 返回。添加和删除服务使用 `tickloop mcp add` 和 `remove`，面板只管理已有条目；`toolExposure` 在 `settings.json` 中编辑。登录页显示链接，本机可以打开浏览器；远程使用时在自己的电脑上打开链接，把完整回调地址粘贴回面板；`Esc` 或 `Ctrl+C` 取消。`/plugins` 的已安装行显示插件带几个 MCP 服务以及是否需要登录。

远程 MCP 服务需要 OAuth 时，运行 `tickloop mcp login <服务>`，在浏览器打开终端打印的链接。Tickloop 可以自动接收本机回调；通过 SSH 使用时，也可以把浏览器跳转后的完整地址粘贴回终端。Ctrl+C 取消，不覆盖已有登录；终端回调输入支持 Ctrl+G 打开外部编辑器。插件服务支持 `plugin:<插件>:<服务>` 和 `plugin_<插件>_<服务>` 两种写法。

在 `/mcp` 登录后立即连接该服务：Agent 运行时由 Agent 连接，启动前由面板连接；其他方式登录从下一次 Agent 请求起生效。令牌自动续期。`tickloop mcp logout <服务>` 在服务器支持时撤销令牌，并删除本地令牌、保留客户端注册。凭据只保存在本机私有的 `credentials/_mcp/` 或 `credentials/<插件>/mcp/` 目录中；服务地址变化后不使用旧令牌。配置固定 `Authorization` header 的服务不走 OAuth。远程服务支持 `oauth.clientId`、`oauth.callbackPort` 和 `authServerMetadataUrl`（也接受放在 `oauth` 内）。

服务配置中的 `${VAR}`、`${VAR:-默认值}` 使用 Agent 的启动环境；缺变量且无默认值时，该服务不启动。`/mcp` 与 CLI 登录使用各自进程的环境。如果 URL 变量与 Agent 启动时的值不同，登录会保存到另一个地址，Agent 无法使用；请让两个终端使用相同的值。模型会请用户添加、配置或登录服务，不会自行执行这些操作。

### 插件用户配置

插件可声明 `userConfig`（字符串、数字、布尔、目录或文件）。安装、启用时在终端询问尚未填写的项；无终端时提示运行 `tickloop plugin configure NAME`。此命令与 `/plugins` 的 Configure 操作均可重新配置。敏感输入不回显或以 `*` 遮掩，`Ctrl+G` 不打开外部编辑器；值保存到仅当前用户可读写的 `credentials/NAME/user-config.json`；其他值保存在 `settings.json` 的 `plugins.NAME.options`。卸载保留这两类配置。MCP 配置使用实际值；Skill 使用非敏感值，敏感值替换为占位文字。修改从下一次请求起生效。

命令和参数或 URL 相同时，优先保留用户自己配置的服务；插件之间按插件名排序，保留在前者，并提示跳过的重复项。连接时替换插件根和数据目录变量（`${TICKLOOP_PLUGIN_ROOT}`、`${TICKLOOP_PLUGIN_DATA}` 及其 `CLAUDE_` 别名），以及启动环境中的 `${VAR}`、`${VAR:-默认值}`。引用了缺失的 `${user_config.*}` 值时，服务显示需要配置且不启动。HTTP 是否需要登录在连接时发现，静态验证不能确定。

旧 Tick 包在首次使用时自动迁移，保留登录；损坏的包会提示重装，旧目录保留。

## 运行环境

- macOS（Apple 芯片或 Intel）或 Linux（ARM64 或 x86_64）。
- 支持真彩色与 UTF-8 的终端。Ghostty、kitty、WezTerm 或 iTerm2 可使用 `ctrl+enter` 等额外按键。

设置、登录与历史保存在 `~/.tickloop`。

## 参与贡献

欢迎提交问题、想法和 Pull Request，详见 [CONTRIBUTING.md](CONTRIBUTING.md)。从源码构建请看其中的开发环境一节。

## 安全

请按 [SECURITY.md](SECURITY.md) 私下报告漏洞，不要发在公开 Issue 中。

## 许可证

[MIT](LICENSE)。第三方代码及其许可证见 [THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md)。
