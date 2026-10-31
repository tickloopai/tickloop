# 插件

[Tickloop](../../README.zh-CN.md) · [English](../plugins.md) | 简体中文

插件为 Agent 增加 Skill、[MCP 服务](mcp.md)和 [Hook](hooks.md)。用 `/plugins` 浏览不带 Tick 程序的已安装和可安装插件，或在命令行中操作：

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

## Marketplaces

在 `/plugins` 的 **Marketplaces** 页或命令行中登记其他来源，`/ticks` 和 `/plugins` 都会读取：

```sh
tickloop plugin marketplace add OWNER/REPO
tickloop plugin marketplace list
tickloop plugin marketplace update MARKETPLACE  # 省略名称刷新全部
tickloop plugin marketplace remove MARKETPLACE
```

`add` 也接受 `OWNER/REPO#REF`、Git 地址（HTTPS、SSH 或 file）、本地目录或 HTTPS 上的 marketplace JSON。名称取自 marketplace 文件。删除 marketplace 不会卸载已安装的插件。浏览使用上次取得的 Git 目录，用 `marketplace update` 刷新；`tickloop` 目录每次实时读取。插件 `update` 会刷新来源并检查 Git 提交，所以显示版本不变时也能更新内容。全部更新会跳过来自已删除 marketplace 的插件。Git 来源带 `sha` 时始终安装该提交，即使 `ref` 已前进；ref 只作为获取线索。提交不存在时安装失败，原插件保持不变。

## 本地插件

本地插件目录或 `.tar.gz` 压缩包：

```sh
tickloop plugin validate ./my-plugin
tickloop plugin install --path ./my-plugin
```

本地路径安装的插件只能重新运行 `install --path` 更新，同版本也会重装；`update` 和面板不更新它们。`validate` 报告清单、组件和路径警告；安装还会检查文件树。安装失败保留原安装。插件标为不可用时，从原来源重装；看不出种类的不可用插件同时列在 `/ticks` 和 `/plugins` 中。

## /plugins 页面

`/plugins` 的用法与 `/ticks` 相同：已安装和可安装两页，`Enter` 打开操作（启用/禁用、更新）或安装，`/` 过滤，同一行按两次 `x` 卸载。已安装行还显示暂不支持的组件，以及插件有几个 Hook 待确认。取不到的 marketplace 各自显示最多三行的错误摘要，其他来源和已安装页仍可用。

`/plugins` 的第三页 **Marketplaces** 完成 `tickloop plugin marketplace` 能做的事。每行显示名字、来源、上次更新时间和是否内置；读取失败的 marketplace 在该行下显示错误。按 `a` 打开单行输入，接受与 `marketplace add` 相同的来源；`Enter` 添加，`Ctrl+G` 打开外部编辑器，`Esc` 取消。在某行按 `Enter` 重新读取该 marketplace，与 `marketplace update NAME` 相同。在同一行 3 秒内按两次 `x` 删除，已安装的插件保留；`tickloop` 行不显示 `x remove`，按 `x` 无操作也无提示；默认的 `claude-plugins-official` 可以删除，结果提示说明如何加回。添加、刷新和删除在后台进行，可安装页随之更新。`/ticks` 没有这一页，提示改到 `/plugins` 管理。

## Skill

输入 `/` 可发现当前工作目录可由用户调用的 Skill，包括内置、用户、项目以及已启用插件和 Tick 的 Skill。Skills 分组显示名称、简短说明和来源；用 `Ctrl+N` / `Ctrl+P` 选择，再按 `Enter` 或 `Tab` 补全。Skill 文件或插件变化后，候选会自动刷新。用 `/plugin:skill [参数]` 调用；短名唯一时也可用 `/skill [参数]`。短名重名时会列出可用的完整名，完整名优先，内置命令始终优先。每次调用显示 `Skill · 名称 · 来源` 行，重开历史仍可见。

已启用插件的 Skill 以 `plugin:skill` 命名，从下一次模型请求起可用，无需重启。禁用会隐藏 Skill；重装或更新后仍保持禁用。安装放置文件；已启用的 MCP 服务随 Agent 启动，或从下一次请求起运行。插件 Skill 和 MCP 工具的调用照常审批。本版本支持 Skill、转为 Skill 的 Commands、Tick 程序、MCP 服务、Hook 和插件用户配置；Hook 在安装和 `tickloop plugin list` 中显示几个待确认，并警告 Tickloop 跳过的事件、类型与字段（见 [Hook](hooks.md)）；其余组件列为暂不支持。卸载插件时一并清除其 Hook 的信任记录。npm、pip 插件来源可以列出但不能安装。安装、更新、卸载和登录由用户发起，模型不会自行执行。

## Commands

插件的 Markdown Commands 同样作为 Skill 使用：输入 `/plugin:command`，或由模型通过 `Skill` 工具按完整名字调用。清单 `commands` 可指定文件、目录或内联正文；未声明时递归扫描 `commands/`。子目录加入冒号命名空间，同目录的 `SKILL.md` 优先于其它 Markdown 文件；Command 与 Skill 完整名相同时，Command 优先。`argument-hint` 显示在补全中；`user-invocable: false` 禁止用户直接调用；`disable-model-invocation: true` 禁止模型调用。没有显式 description 或 `when_to_use` 的 Command 不列入模型目录，但仍可按完整名字调用。

Commands 替换 `$ARGUMENTS`、零基位置参数 `$0`、`$1`、`$ARGUMENTS[0]`，以及 `arguments` 声明的命名参数；引号内参数保持为一个参数，缺少的位置参数保留占位符。没有成功替换占位符时，非空参数追加到正文。以 ! 加反引号或 ` ```! ` 围栏书写的 Shell 块，只在调用时通过普通 Bash 准备和审批执行；成功完成后的输出才进入指导。失败或结果未知会停止准备，不自动重试。安装、发现和补全均不执行这些块。用户调用先经过 `SessionStart` 和 `UserPromptSubmit`，再启用指导或运行 Shell 块。重启后，只复用能够证明已为本次调用准备好且不需要 Shell 的指导；准备记录缺失时须显式重新调用命令。

`allowed-tools` 警告并忽略，不授予权限。`model`（含 `inherit`）警告并忽略，保持当前 Chat 模型。`@文件` 保留为文字，Agent 通过 `Read` 在正常权限下读取。不支持的字段（包括 `name` 别名、`context`、`agent`、`background`、`effort`、`shell` 和 frontmatter `hooks`）警告后忽略；调用开关必须是 YAML 布尔值。只读取已启用插件的 Commands，不扫描用户或项目的 `.claude/commands`。

## 插件用户配置

插件可声明 `userConfig`（字符串、数字、布尔、目录或文件）。安装、启用时在终端询问尚未填写的项；无终端时提示运行 `tickloop plugin configure NAME`。此命令与 `/plugins` 的 Configure 操作均可重新配置。敏感输入不回显或以 `*` 遮掩，`Ctrl+G` 不打开外部编辑器；值保存到仅当前用户可读写的 `credentials/NAME/user-config.json`；其他值保存在 `settings.json` 的 `plugins.NAME.options`。卸载保留这两类配置。MCP 配置使用实际值；Skill 使用非敏感值，敏感值替换为占位文字。修改从下一次请求起生效。

命令和参数或 URL 相同时，优先保留用户自己配置的服务；插件之间按插件名排序，保留在前者，并提示跳过的重复项。连接时替换插件根和数据目录变量（`${TICKLOOP_PLUGIN_ROOT}`、`${TICKLOOP_PLUGIN_DATA}` 及其 `CLAUDE_` 别名），以及启动环境中的 `${VAR}`、`${VAR:-默认值}`。引用了缺失的 `${user_config.*}` 值时，服务显示需要配置且不启动。HTTP 是否需要登录在连接时发现，静态验证不能确定。

旧 Tick 包在首次使用时自动迁移，保留登录；损坏的包会提示重装，旧目录保留。
