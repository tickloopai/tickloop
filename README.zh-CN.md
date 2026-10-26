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
| `/plugins`、`/artifacts` | 管理插件；登录 Artifacts |
| `/exit` | 离开界面（Agent 继续运行） |

按键：

- `enter` 发送消息。Agent 工作时消息会等到下一步再处理；`ctrl+enter` 打断并立即发送。
- `esc` 打断 Agent。输入为空时连按两次，可撤回到之前的某条消息。
- `shift+tab` 在 Auto 与 Plan 模式之间切换。
- `ctrl+j` 换行；`ctrl+g` 用外部编辑器编辑消息。
- 列表和菜单中用 `j`／`k` 或方向键移动，`enter` 选择，`esc` 返回。
- 在 Monitor 中 `enter` 打开 Tick，`esc` 再隐藏；程序会继续运行。
- 在 Monitor 同一行三秒内按两次普通 `x`，可关闭或移除 Tick、停止 Shell、解除 Artifact 关联或删除定时任务。第一次只在按键提示行显示红色 `Press x again to …`，没有确认弹窗；超时、移动选择、按其他键或离开会取消。`Ctrl+X` 不执行这些操作；文本输入和筛选框中的 `x` 仍是文字。

## 插件与数据源

插件为 Agent 增加 Skill 和 Tick 程序。用 `/plugins` 浏览已安装和可安装的插件，或在命令行中操作：

```sh
tickloop plugin list --available
tickloop plugin install NAME@MARKETPLACE
tickloop plugin list
tickloop plugin login NAME                  # 插件需要登录时使用
tickloop plugin logout NAME
tickloop plugin update NAME                 # 省略 NAME 更新所有 marketplace 插件
tickloop plugin disable NAME
tickloop plugin enable NAME
tickloop plugin uninstall NAME              # 删除插件数据，保留登录
tickloop plugin uninstall NAME --keep-data  # 同时保留插件数据
```

内置 marketplace 为 `tickloop` 和 `claude-plugins-official`，不能删除；Git marketplace 在首次使用时才下载。官方 Tick 包在 [tickloopai/ticks](https://github.com/tickloopai/ticks)。来源明确时可只写 `NAME`；多个 marketplace 提供同名插件时，命令列出候选，已有安装则优先使用其来源。要把已安装的同名插件换到另一个 marketplace，先卸载。

添加其他来源只在命令行中操作：

```sh
tickloop plugin marketplace add OWNER/REPO
tickloop plugin marketplace list
tickloop plugin marketplace update MARKETPLACE  # 省略名称刷新全部
tickloop plugin marketplace remove MARKETPLACE
```

`add` 也接受 `OWNER/REPO#REF`、Git 地址（HTTPS、SSH 或 file）、本地目录和 HTTPS marketplace JSON 地址，名称取自 marketplace 声明。删除 marketplace 会保留从中安装的插件。浏览使用上次取得的 Git 目录，用 `marketplace update` 刷新；`tickloop` 目录每次读取。插件 `update` 会刷新来源，并按 Git 提交判断更新，因此显示版本不变也能更新内容。更新全部时跳过来源已被删除的插件。

本地插件目录或 `.tar.gz` 压缩包：

```sh
tickloop plugin validate ./my-plugin
tickloop plugin install --path ./my-plugin
```

本地路径安装的插件只能重新运行 `install --path` 更新，同版本也会重装；`update` 和面板不更新它们。`validate` 报告清单、组件和路径警告；安装还会检查文件树。安装失败保留原安装。插件标为不可用时，从原来源重装。

在 `/plugins` 中，用 `Tab` 或左右键切换 **Installed**（已安装）和 **Available**（可安装），`j`/`k` 或 `Ctrl+N`/`Ctrl+P` 选择，`Enter` 安装或打开操作。已安装行显示来源、版本、启用状态、登录和暂不支持的组件，可启用/禁用、更新、登录/退出登录。可安装页按 `/` 开始过滤名字、说明和 marketplace，不区分大小写；过滤时 `j`/`k` 是文字，`Ctrl+N`/`Ctrl+P` 选择，`Ctrl+G` 打开外部编辑器。`Esc` 退出过滤并保留查询，再按一次关闭面板。取不到的 marketplace 各自显示最多三行的错误摘要，其他来源和已安装页仍可用。

在已安装页同一行三秒内按两次 `x` 卸载：默认删除插件数据，保留登录，不停止运行中的 Tick。移动选择、按其他键、离开或超时都会取消确认。保留数据请用命令行的 `--keep-data`。登录保留在配置目录的 `credentials/NAME/`；要删除保存的登录，先 `logout` 再卸载。

输入 `/` 可发现当前工作目录可由用户调用的 Skill，包括内置、用户、项目和已启用插件的 Skill。Skills 分组显示名称、简短说明和来源；用 `Ctrl+N` / `Ctrl+P` 选择，再按 `Enter` 或 `Tab` 补全。Skill 文件或插件变化后，候选会自动刷新。用 `/plugin:skill [参数]` 调用；短名唯一时也可用 `/skill [参数]`。短名重名时会列出可用的完整名，完整名优先，内置命令始终优先。每次调用显示 `Skill · 名称 · 来源` 行，重开历史仍可见。

已启用插件的 Skill 以 `plugin:skill` 命名，从下一次模型请求起可用，无需重启。禁用会隐藏 Skill，并阻止按名字打开该插件的 Tick；重装或更新后仍保持禁用。安装不运行程序，也不增加权限。Tick 程序在按你的权限设置打开时才以你的身份运行；插件 Skill 的工具调用照常审批。本版本只有 Skill 和 Tick 组件生效，其余列为暂不支持；npm、pip 插件来源可以列出但不能安装。安装、更新、卸载和登录由用户发起，模型不会自行执行。

旧 Tick 包在首次使用时自动迁移，保留登录；损坏的包会提示重装，旧目录保留。`tickloop tick …` 和 `/ticks` 本版本仍可作为别名使用，并提示改名。

程序实现 [Tick 协议](tick/protocol/SPECIFICATION.md) 就能成为扩展 Tick；可从 [Counter 示例](tick/examples/counter.py) 开始。

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
