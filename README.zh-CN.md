<h1>
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="assets/brand/lockup-on-dark.svg">
    <img src="assets/brand/lockup.svg" alt="Tickloop" width="265" height="50">
  </picture>
</h1>

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
| `/ticks`、`/artifacts` | 添加数据源；登录 Artifacts |
| `/exit` | 离开界面（Agent 继续运行） |

按键：

- `enter` 发送消息。Agent 工作时消息会等到下一步再处理；`ctrl+enter` 打断并立即发送。
- `esc` 打断 Agent。输入为空时连按两次，可撤回到之前的某条消息。
- `shift+tab` 在 Auto 与 Plan 模式之间切换。
- `ctrl+j` 换行；`ctrl+g` 用外部编辑器编辑消息。
- 列表和菜单中用 `j`／`k` 或方向键移动，`enter` 选择，`esc` 返回。
- 在 Monitor 中 `enter` 打开 Tick，`esc` 再隐藏；程序会继续运行。

## 数据源

Tick 包为 Agent 接入更多数据。用 `/ticks` 浏览和安装，或在命令行中：

```sh
tickloop tick list --available
tickloop tick install PACKAGE
tickloop tick login PACKAGE
```

安装 Tick 包只是把它加进来：在 Agent 按你的权限设置打开它之前，不会运行，也不会获得任何访问权限。官方 Tick 包在 [tickloopai/ticks](https://github.com/tickloopai/ticks)。

任何程序只要实现 [Tick 协议](tick/protocol/SPECIFICATION.md)（英文）就能成为 Tick，可以从 [counter 示例](tick/examples/counter.py) 开始。

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
