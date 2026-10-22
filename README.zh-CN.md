<h1>
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="assets/brand/lockup-on-dark.svg">
    <img src="assets/brand/lockup.svg" alt="Tickloop" width="265" height="50">
  </picture>
</h1>

[官网](https://tickloop.ai) · [English](README.md) | 简体中文

Tickloop 是面向代码与办公的终端 AI Agent，可以理解项目、修改代码、运行测试，整理资料、撰写文档、制作页面与演示，并根据执行结果和反馈继续推进工作。它的独特之处是 **Tick**：人和 Agent 同时使用的一个运行中的终端程序。Agent 可以读取 Tick、调用它声明的工具，并通过订阅让程序中的变化唤醒 Agent 的 Loop，推动工作继续。

主命令是 `tickloop`（Homebrew 同时安装短命令 `tl`）。

## 功能

- **Tick。** 一次运行的程序是一个 Tick Instance。**扩展 Tick** 讲 [Tick 协议](tick/protocol/SPECIFICATION.md)，提供结构化的状态、工具和更新；其他交互式程序（日志查看、REPL 等）可以作为 **Terminal Tick** 打开，由 Agent 读取屏幕文字并输入；后台 Shell 命令以 **Shell Tick** 运行。
- **Tickhost。** 本机 Tick 全部由 Tickhost 运行，它由 Agent daemon 按需拉起；离开或关闭视图时 Tick 继续运行。对话旁的 Monitor 列出当前 Chat 的 Tick、订阅、Artifact 与后台 Shell，对某行按 Enter 打开完整视图，Esc 隐藏。
- **Herdr Pane。** 在 [Herdr](https://github.com/herdrdev/herdr) 中，Tickloop 可以把 Tick 显示在 Agent 旁边的分屏 Pane 中（如“在你当前 Tab 右边打开 counter Tick”）。Pane 只是视图：移动它保留实例及订阅，关闭它 Tick 继续运行。Herdr 外 Tick 照常工作；tmux、Zellij 的 Pane 中暂不能显示 Tick。
- **订阅推动 Loop。** 订阅属于 Loop，总是明确建立；打开或读取 Tick 不会自动订阅。扩展 Tick 与 Terminal Tick 按高频（每分钟，默认）、中频（10 分钟）或低频（30 分钟）投递，只投递最新状态；Shell Tick 退出时立即投递。
- **多模型。** 内置目录包括 Anthropic Claude、OpenAI（API 与 ChatGPT/Codex 订阅）、GitHub Copilot、Google Gemini 与 Vertex AI、DeepSeek、Kimi、MiniMax、智谱 GLM / Z.ai、xAI 与阿里云。可用 `/model` 在同一 Chat 中途切换模型，历史按目标协议延续。
- **权限与模式。** 工具调用经过权限系统，模式有 Auto（默认，由模型审批）、Ask、Plan 与 Full Access。`shift+tab` 切换模式，`/permissions` 管理规则并查看近期决定。
- **Subagent。** `Subagent` 工具真实委派给子 Agent，可前台等待或后台运行。同一家族的成员可以互发消息，最多嵌套三层，并可按需共享任务列表。
- **Artifact。** Agent 可以创建 Doc、Page 与 Slides，发布到 Tickloop Artifacts 服务，并接收其中的 @tickloop 反馈。用 `/artifacts` 登录。
- **持续运行。** Agent 运行在本机 daemon 中。关闭终端、Pane 或 `/exit` 只离开界面；`/resume` 与 `tickloop -c` 可回到之前的 Chat。

## 运行环境

- macOS（Apple silicon 或 Intel），或 Linux（ARM64 或 x86_64；静态构建，glibc 与 musl 发行版均可运行）。
- 支持真彩色的终端与 UTF-8 locale。支持 kitty 键盘协议的终端（Ghostty、kitty、WezTerm、开启 CSI u 的 iTerm2）还能使用 `ctrl+enter` 等按键。
- 可选：[Herdr](https://github.com/herdrdev/herdr)，用于在 Pane 中显示 Tick。

## 安装

`tickloop` 与 `tickhost` 总是一起安装和升级；daemon 只拉起与自己同目录、同构建的 `tickhost`。

### Homebrew

```sh
brew install tickloopai/tap/tickloop
```

### 安装脚本

```sh
curl -fsSL https://tickloop.ai/install.sh | sh
```

脚本安装到 `~/.local/bin` 与 `~/.local/share/tickloop/versions/`，按当前系统与架构选择构建，固定发行标签并校验 SHA-256；`~/.local/bin` 不在 PATH 中时，会在 shell 配置（`~/.zshrc`、`~/.bashrc` 与 `~/.profile`，或 fish 的 `conf.d`）里加入一次，并列出改动的文件；遇到其他来源的已有安装时报告冲突，不自动接管。脚本源码见 [scripts/install.sh](scripts/install.sh)。

### Cargo 启动器

```sh
cargo install --locked tickloop
```

crates.io 上的 `tickloop` 是一个小型启动器，`cargo install` 期间下载、校验并嵌入同版本的官方 macOS 或 Linux 构建，详见 [distribution/cargo](distribution/cargo/README.md)。

### 从源码安装

需要当前 Rust stable 工具链。

```sh
cargo install --locked --git https://github.com/tickloopai/tickloop \
  tickloop-agent tickloop-tickhost
```

加 `--tag vX.Y.Z` 可固定版本。在源码检出中，`make install` 把同样两个程序安装到 `~/.cargo/bin`。

### 更新

```sh
tickloop update           # 按当前安装来源升级整套程序
tickloop update --check   # 只检查
tickloop update --status  # 查看最近一次自动或主动更新的结果
```

Homebrew、安装脚本与 Cargo 启动器安装会在后台检查更新，不阻塞界面。更新不会停止运行中的 daemon、Agent 或 Tick，新版本在下次启动 Tickloop 时生效。

## 快速开始

1. 在项目目录运行 `tickloop`；要在别处启动，传入 `--cwd PATH`。
2. 打开 `/provider` 选择连接；还没有凭据时会要求输入 API Key 或登录。API Key 保存在 `~/.tickloop/.env`（权限 `0600`），也可以使用 `DEEPSEEK_API_KEY` 等环境变量。默认模型是 DeepSeek。
3. 用自然语言描述要做的事。`/model` 切换模型，`/help` 查看命令，`tickloop -c` 继续本目录最近的 Chat。

不需要任何账户也能体验协议 Tick，对 Agent 说：

> 以 `protocol: true` 打开 `python3 tick/examples/counter.py`，并订阅它。

在 Counter 的视图中（在 Monitor 中对它按 Enter）输入 `+` 后回车即可产生更新，也可以让 Agent 调用 `increment`。订阅它的 Loop 收到新状态后继续执行。

状态、设置、登录与已安装的 Tick 包保存在 `~/.tickloop`；设置 `TICKLOOP_CONFIG_DIR` 可换用其他目录。

## 使用

### 模型

- `/provider` 列出内置连接。选择一个连接后登录或输入 API Key，再选择模型。在连接上按 `p` 打开详情，可替换或删除保存的凭据。
- `/model` 列出所有已配置连接的模型，左右键调整 Thinking；`/thinking` 只调整 Thinking 档位。
- 模型选择与 Thinking 保存在 `~/.tickloop/settings.json`。Provider 与模型来自编译进程序的内置目录，不读取外部 Provider 定义文件。
- 模型请求失败时显示连接、HTTP 状态和（脱敏后的）服务原文。排查网络失败时，可用 `TICKLOOP_NETWORK_DIAGNOSTICS=1` 启动并把标准错误重定向到文件；它记录失败阶段、主机、代理来源、状态与耗时，不记录 URL 路径、请求头或正文。

### 命令

用 `tickloop -h` 或 `tickloop --help` 查看命令行选项；进入 Console 后，`/help` 查看命令与快捷键。`tickloop -p --help`、`tickloop acp --help` 和 `tickloop tick --help` 分别说明这些 CLI 入口。

| 命令 | 用途 |
| --- | --- |
| `/help` | 按功能分组的命令（`/help advanced` 查看其余命令） |
| `/resume`、`/clear`、`/branch` | 打开已保存的 Chat、开始新 Chat，或从当前 Chat 分支 |
| `/plan` | 只读规划，批准计划后再开始工作 |
| `/permissions`、`/settings` | 执行模式与权限规则；模型、Thinking、输出风格等设置 |
| `/rewind`、`/diff` | 把代码和／或对话恢复到之前的提示；审阅文件变更 |
| `/compact`、`/details` | 压缩上下文；浏览与搜索详细会话记录（也可按 `ctrl+o`） |
| `/ticks`、`/artifacts` | 管理 Tick 包；登录并管理 Artifact |
| `/websearch` | 选择网络搜索服务 |
| `/exit` | 关闭当前界面，Agent 继续运行 |

命令行中，`tickloop -c` 继续本目录最近的 Chat，`tickloop -r [CHAT]` 恢复指定 Chat（省略时打开选择器），`--fork-session` 把恢复的 Chat 复制为新 Chat。`tickloop daemon stop` 停止 daemon 及全部运行中的 Agent，保存的历史保留，Tick 继续运行。

### 非交互执行与 ACP

```sh
tickloop -p "概述这个项目"                         # 长选项为 --print，不支持 --p
printf '%s' '解释这段文字' | tickloop -p -
tickloop -p "审查本次修改" --output-format json --timeout 300
tickloop -p "运行检查" --output-format stream-json
tickloop acp                                       # stdin/stdout 上的 ACP JSON-RPC
```

先在 Console 中用 `/provider` 配置连接。这些入口复用已有模型设置、权限规则与 Agent runtime，不打开终端 UI。未信任目录拒绝启动：先在交互界面信任，或显式传入 `--working-directory-is-trusted`，仅声明本次调用可信，不保存长期信任。

Print 默认 Auto + 拒绝人工提示，不是 Full Access。`text` 只输出本次最终回答；`json` 输出版本化结果；`stream-json` 输出版本化 JSONL 事件与最终结果。诊断写 stderr。退出码为 0（成功）、1（执行／结果未知／清理失败）、2（参数错误）、3（需要人工）、124（超时）、130（SIGINT）、143（SIGTERM）。`--no-session-persistence` 不保存新 Chat；Print 不恢复已有 Chat。

首版托管 profile 不提供结构化问答、Plan 批准、Subagent、独立／定时目标、Tick 订阅及持续 Artifact 反馈。普通工具拒绝不一定导致整个任务失败。回合结束后先收束并清理残留 Shell 与 hosted Tick，再报告成功；失败与结果未知不自动重放修改。客户端崩溃由 daemon 控制租约到期回收兜底，不改变普通 Console 离开视图后继续运行的行为。

在支持 ACP 的编辑器中配置启动 `tickloop acp`。稳定 ACP v1 支持连续提示、文本／工具更新、一次性审批、取消、资源引用／嵌入文本，以及所选模型支持时的内联图片。资源 URI 仅表示来源，不授予读取或抓取权限。会话配置提供真实可用模型及启动权限上限内的 Auto／Ask；公布的斜杠命令来自真实可由用户调用的 Skill。持久 ACP 会话支持列表、独占装载及公开历史回放；装载不调用模型、工具或 MCP，不能接管活 Chat 或普通 Console 历史。Print 仍不支持恢复。

客户端 stdio MCP 服务在新建会话时只验证，首次 prompt 经强制一次性人工审批后才启动（Full Access 仍然如此，显式 Deny 优先）。服务使用干净的最小环境加客户端显式变量，跨 prompt 复用，在 release／EOF／租约到期时关闭。argv/env 正文不落公开历史；审批时 argv 瞬时发回提供它的客户端，因此秘密应放在 env 而非 argv。MCP 进程不是沙箱；监督进程负责清理进程组，但不能保证恶意 `setsid` 逃逸或监督进程同时被强杀时的后代清理。不支持 HTTP／SSE MCP 或客户端文件／终端执行。

客户端传入 `cwd` 本身不代表目录可信。每次提示最多 64 块、256 KiB 普通文本、1 MiB 资源文本、4 张图片（base64 合计 16 MiB）；协议帧与队列同样有界。stdout 仅传 ACP 协议。ACP 是 AgentAccess 客户端，不是 Tick 类型。

### 按键

- `shift+tab` 切换执行模式。`enter` 发送；Agent 工作时消息排队，在下一个工具边界并入。`ctrl+enter` 中断并立即发送。
- `ctrl+j`（或终端能识别的 `alt+enter`／`shift+enter`）插入换行，`ctrl+g` 用外部编辑器编辑输入。文本输入没有 Vim 模式。
- 空输入时 `esc esc` 打开 Rewind。前台 `Bash` 运行满两秒后，`ctrl+b` 把它转入后台。
- 列表与菜单用 `j`／`k` 或 `ctrl+n`／`ctrl+p` 导航，`enter` 确认，`esc` 返回一层。
- 在 Monitor 中，`enter` 打开 Tick 或 Shell 的视图（Herdr 内为浮窗，否则全屏）。视图中 `esc` 隐藏视图、程序继续运行，`ctrl+b esc` 把 Esc 发给程序。`s` 停止运行中的 Shell，`x` 确认后关闭或移出该行。

## Tick 包

Tick 包提供数据来源，每个包是一个预编译可执行文件、包清单 `tick-package.json`，以及教 Agent 使用它的 Skill。官方包在 [tickloopai/ticks](https://github.com/tickloopai/ticks) 中开发和发布，那里同时是 Tick 市场。

```sh
tickloop tick list                 # 已安装的包及登录状态
tickloop tick list --available     # 市场中的包、版本与平台支持
tickloop tick install PACKAGE
tickloop tick login PACKAGE        # 设备码登录；有多个 issuer 时用 --issuer NAME
tickloop tick logout PACKAGE
tickloop tick uninstall PACKAGE
tickloop tick install --path DIR   # 本地包目录或 .tar.gz，供开发包时使用
```

在 Agent 中，`/ticks` 提供同样的安装、更新、卸载、登录与退出登录。OAuth 登录打印验证链接与验证码，在任意设备的浏览器中确认即可，远程主机上也能完成；API Key 登录以不回显方式读入密钥。登录状态有 `logged in`、`needs login`、`login expired`、`no login needed` 与 `login unknown`。

安装包只放置程序与 Skill：不运行包里的程序，不打开、不订阅 Tick，也不授予任何数据、账户或其他权限。凭据保存在 `~/.tickloop/credentials/<包>/`，只交给该包自己的进程，`uninstall` 时保留。安装后的包从 Agent 的下一次请求起可用，其 Skill 以 `<包>:<skill>` 出现，如 `<包>:usage`。

## Tick 协议

扩展 Tick 讲 Tick 协议 3：在 stdin/stdout 上按行 JSON-RPC 2.0，方法为 `tick/describe`、`tick/read`、`tick/call` 与通知 `tick/update`，程序界面画在自己的控制终端上。任何程序实现它就能成为 Tick。

- [Tick 协议规格](tick/protocol/SPECIFICATION.md)（[English](tick/protocol/SPECIFICATION.en.md)）
- [Counter 示例](tick/examples/counter.py)
- Rust 的 Tick 侧 SDK：[`tick/sdk`](tick/sdk)

## 文档

- [Tick 协议规格](tick/protocol/SPECIFICATION.md)
- [领域词汇](CONTEXT.md)
- [贡献指南](CONTRIBUTING.md)（英文）与 [Agent 工作规则](CLAUDE.md)

## 参与贡献

欢迎报告问题、提出想法和提交 Pull Request。构建、测试与评审流程见 [CONTRIBUTING.md](CONTRIBUTING.md)（英文）。新的 Tick 包或对官方包的修改请提交到 [tickloopai/ticks](https://github.com/tickloopai/ticks)。

## 安全

请不要在公开 Issue 中报告漏洞，私下报告的方式见 [SECURITY.md](SECURITY.md)。

## 许可证

Tickloop 以 [MIT 许可证](LICENSE)发布。改编的第三方代码及其许可证见 [THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md)。
