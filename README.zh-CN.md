# Tickloop

[官网](https://tickloop.ai) · [English](README.md) | 简体中文

[![crates.io](https://img.shields.io/crates/v/tickloop)](https://crates.io/crates/tickloop)
[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](LICENSE)

Tickloop 是在终端里工作的 AI Agent。用日常语言告诉它要做什么：它会读懂你的项目、修改代码、运行测试，整理资料、撰写文档、制作页面和演示，并一直推进到完成。

## 为什么是 Tick

它的独特之处是 **Tick**：你和 Agent 一起看着的运行中程序，比如日志查看器、看板或一条长时间运行的命令。Agent 可以读取 Tick、操作它，并在其中有变化时被唤醒。

从 [Tick](docs/zh-CN/ticks.md) 安装，或自己实现协议（[编写 Tick](docs/zh-CN/writing-ticks.md)）。下面的快速开始会让 Agent 打开 [Counter 示例](tick/examples/counter.py)。

## 功能

- **交给它真正的工作。** Agent 修改文件、运行命令并检查结果。默认是 Auto 模式：常规操作由审查模型放行，有风险的操作会问你。[权限](docs/zh-CN/permissions.md)
- **帮你盯着变化。** 把程序作为 Tick 打开并订阅，它一变化 Agent 就会响应。对话旁的 Monitor 列出你的 Tick、后台命令和定时任务。[Tick](docs/zh-CN/ticks.md) · [界面](docs/zh-CN/interface.md)
- **定时重复。** `/loop 5m 检查部署` 每五分钟执行一次；`/goal` 设一个目标，Agent 持续推进直到达成。
- **选你喜欢的模型。** 内置 Claude、OpenAI（API 或 ChatGPT 订阅）、GitHub Copilot、Gemini、DeepSeek、Kimi、MiniMax、GLM、xAI 和阿里云，随时用 `/model` 切换。
- **分享成果。** 把文档、网页和演示发布到 Tickloop Artifacts，并收到他人的反馈。`/artifacts` 用来登录并把它们加入对话。
- **离开再回来。** 关闭终端不会停止 Agent，`tickloop -c` 接着最近的对话继续。[Agents 总览](docs/zh-CN/interface.md#agents-总览)

## 运行环境

- macOS（Apple 芯片或 Intel）或 Linux（ARM64 或 x86_64）。
- 支持真彩色与 UTF-8 的终端。Ghostty、kitty、WezTerm 或 iTerm2 可使用 `ctrl+enter` 等额外按键。

设置、登录与历史保存在 `~/.tickloop`。

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

## 快速开始

1. 在项目目录运行 `tickloop`。
2. 首次启动时选择模型服务商，输入 API Key 或登录，再选择模型。
3. 描述你要完成的事。

想看看 Tick 怎么用？对 Agent 说：

> 以 `protocol: true` 打开 `python3 tick/examples/counter.py`，并订阅它。

然后在 Monitor 中选中 Counter，按 Enter 进入它的卡片，输入 `+` 再按 Enter：Agent 会收到新的计数并作出回应。连按两次 `esc` 离开卡片。

## 常用命令

命令行选项见 `tickloop --help`。

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
| `/hooks` | 查看当前目录的 Hook，信任、停用或恢复 |
| `/context` | 按类别查看上下文占用，以及工具搜索能找到多少 MCP 工具；`/context all` 列出每一项 |
| `/artifacts` | 登录 Artifacts，把 Artifact 加入对话 |
| `/exit` | 离开界面（Agent 继续运行） |

- `enter` 发送消息。Agent 工作时消息会等到下一步再处理；`ctrl+enter` 打断并立即发送。
- `esc` 打断 Agent。输入为空时连按两次，可撤回到之前的某条消息。
- `shift+tab` 在 Auto 与 Plan 模式之间切换。
- `ctrl+j` 换行；`ctrl+g` 用外部编辑器编辑消息。
- 列表和菜单中用 `ctrl+n`／`ctrl+p` 或方向键移动，`enter` 选择，`esc` 返回。

Monitor、全屏的 Tick 与 Shell、`tickloop attach` 和 Agents 总览见 [界面](docs/zh-CN/interface.md)。

## 模式

执行模式决定 Agent 如何获得操作许可。**Auto**（默认）由审查模型批准常规操作，有风险的才问你；**Ask** 允许读取和编辑工作目录内的文件，其他受保护操作逐次询问；**Plan** 只做计划；**Full Access** 跳过普通审批，但显式 Ask 与 Deny 规则仍然生效。可用 `/permissions`、`/mode` 或 `shift+tab` 切换。单次启动的规则、附加目录，以及从 Claude Code 导入，见 [权限](docs/zh-CN/permissions.md)。在 macOS 上，Bash 和 Shell 可以在[沙箱](docs/zh-CN/sandbox.md)中运行。

## 终端之外

```sh
tickloop -p "概述这个项目"    # 长选项为 --print，不支持 --p
```

```sh
tickloop acp                  # stdin/stdout 上的 ACP JSON-RPC
```

Print 与 ACP 复用已保存的模型、权限规则和 Agent runtime，不打开终端界面。输出格式、退出码和编辑器配置见 [非交互执行与 ACP](docs/zh-CN/print-and-acp.md)。

## 文档

| 指南 | 内容 |
| --- | --- |
| [界面](docs/zh-CN/interface.md) | 按键、Monitor、Agents 总览、上下文压缩 |
| [权限](docs/zh-CN/permissions.md) | 执行模式、启动规则、附加目录、从 Claude Code 导入 |
| [沙箱](docs/zh-CN/sandbox.md) | macOS 上 Bash 与 Shell 的沙箱 |
| [非交互执行与 ACP](docs/zh-CN/print-and-acp.md) | 非交互执行与编辑器集成 |
| [指令](docs/zh-CN/instructions.md) | `AGENTS.md`、`CLAUDE.md` 与 `@path` 引入 |
| [Tick](docs/zh-CN/ticks.md) | 安装、登录并打开 Tick |
| [编写 Tick](docs/zh-CN/writing-ticks.md) | 协议、宽度与按键 |
| [插件](docs/zh-CN/plugins.md) | 插件、marketplace、Skill、命令、用户配置 |
| [MCP](docs/zh-CN/mcp.md) | MCP 服务、工具搜索、登录 |
| [Hook](docs/zh-CN/hooks.md) | command Hook 与信任 |
| [配置](docs/zh-CN/configuration.md) | 本手册中的全部 `settings.json` 键 |

## 参与贡献

欢迎提交问题、想法和 Pull Request，详见 [CONTRIBUTING.md](CONTRIBUTING.md)。从源码构建请看其中的开发环境一节。

## 安全

请按 [SECURITY.md](SECURITY.md) 私下报告漏洞，不要发在公开 Issue 中。

## 许可证

[MIT](LICENSE)。第三方代码及其许可证见 [THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md)。
