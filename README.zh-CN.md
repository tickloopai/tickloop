<h1>
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="assets/brand/lockup-on-dark.svg">
    <img src="assets/brand/lockup.svg" alt="Tickloop" width="265" height="50">
  </picture>
</h1>

[官网](https://tickloop.ai) · [English](README.md) | 简体中文

Tickloop 是面向代码与办公的终端 AI Agent，可以理解项目、修改代码、运行测试，整理资料、撰写文档、制作页面与演示，并根据执行结果和反馈继续推进工作。它的独特之处是 **Tick**：人和 Agent 同时使用的一个运行中的终端程序。Agent 可以读取 Tick、调用它声明的工具，并通过订阅让程序中的变化唤醒 Agent 的 Loop，推动工作继续。

主命令是 `tickloop`。Homebrew、安装脚本与 Cargo 启动器同时安装短命令 `tl`。

## 功能

- **Tick。** 一次运行的程序是一个 Tick Instance。**扩展 Tick** 讲 [Tick 协议](tick/protocol/SPECIFICATION.md)，提供结构化的状态、工具和更新；其他交互式程序（日志查看、REPL 等）可以作为 **Terminal Tick** 打开，由 Agent 读取屏幕文字并输入；后台 Shell 命令以 **Shell Tick** 运行。
- **Tickhost。** 本机 Tick 全部由 Tickhost 运行，它由 Agent daemon 按需拉起；离开或关闭视图时 Tick 继续运行。对话旁的 Monitor 列出当前 Chat 的 Tick、订阅、Artifact、后台 Shell 与定时任务，对某行按 Enter 打开完整视图，Esc 隐藏。
- **Herdr Pane。** 在 [Herdr](https://github.com/herdrdev/herdr) 中，Tickloop 可以把 Tick 显示在 Agent 旁边的分屏 Pane 中（如“在你当前 Tab 右边打开 counter Tick”）。Pane 只是视图：移动它保留实例及订阅，关闭它 Tick 继续运行。Herdr 外 Tick 照常工作；tmux、Zellij 的 Pane 中暂不能显示 Tick。
- **订阅推动 Loop。** 订阅属于 Loop，总是明确建立；打开或读取 Tick 不会自动订阅。扩展 Tick 与 Terminal Tick 按高频（每分钟，默认）、中频（10 分钟）或低频（30 分钟）投递，只投递最新状态；Shell Tick 退出时立即投递。
- **多模型。** 内置目录包括 Anthropic Claude、OpenAI（API 与 ChatGPT/Codex 订阅）、GitHub Copilot、Google Gemini 与 Vertex AI、DeepSeek、Kimi、MiniMax、智谱 GLM / Z.ai、xAI 与阿里云。可用 `/model` 在同一 Chat 中途切换模型，历史按目标协议延续。
- **权限与模式。** 工具调用经过权限系统，模式有 Auto（默认，由模型审批）、Ask、Plan 与 Full Access。`shift+tab` 在 `/settings` → `shift+tab modes` 中勾选的模式间切换（默认 Auto 与 Plan）。`/permissions` 选择执行模式、配置 Auto 审批并查看近期决定；高级命令 `/rules` 列出权限规则，`/rule` 添加或删除一条。
- **Subagent。** `Subagent` 工具真实委派给子 Agent，可前台等待或后台运行。同一家族的成员可以互发消息，最多嵌套三层，并可按需共享任务列表。
- **Artifact。** Agent 可以创建 Doc、Page 与 Slides，发布到 Tickloop Artifacts 服务，并接收其中的 @tickloop 反馈。用 `/artifacts` 登录。
- **持续运行。** Agent 运行在本机 daemon 中。关闭终端、Pane 或 `/exit` 只离开界面；`/resume` 与 `tickloop -c` 可回到之前的 Chat。

## 定时任务

用 `/loop 5m 检查部署状态` 按固定间隔执行（创建后先执行一次，之后按计划执行），或用 `/loop 检查部署状态` 让 Agent 在每轮结束时选择下次唤醒时间并说明理由。间隔可写在开头（`30m`）或结尾（`every 2 hours`）。秒向上取整到分钟，无法均匀映射到 cron 的间隔会采用最接近的可用间隔并告知实际值。

`/loop` 与 `/loop 5m` 不写指令时，每次运行重新读取项目 `.tickloop/loop.md`，其次配置根的 `loop.md`（最多 25,000 字节），都没有则继续未完成工作、照看当前分支 PR、清理本次工作涉及的代码。没有 Folder 的 Chat 只读取配置根的 `loop.md`。维护指令不授权新工作或未经允许的不可逆操作。

Monitor 底部为每个定时任务显示两行（`◷ 节奏 · 下次时间` 与指令；最多 3 个，其余收在 `+N more`），下次触发前 30 分钟内显示进度。`enter` 查看 cron、完整指令、创建与到期时间、跳过次数；连按两次 `ctrl+x` 删除，`p` 暂停或恢复。也可以直接让 Agent 查看、修改或删除任务，或创建一次性提醒。重复任务在 7 天后最后执行一次并删除。

`esc` 与 `/stop` 都结束自定节奏的 `/loop` 并删除该任务，固定间隔与一次性任务继续。`/stop` 还暂停接收，发送新消息或 Continue 恢复。离开界面后，定时任务仍由本机 daemon 运行。

## 运行环境

- macOS（Apple silicon 或 Intel），或 Linux（ARM64 或 x86_64；静态构建，glibc 与 musl 发行版均可运行）。
- 支持真彩色的终端与 UTF-8 locale。支持 kitty 键盘协议的终端（Ghostty、kitty、WezTerm、开启 CSI u 的 iTerm2）还能使用 `ctrl+enter` 等按键。
- 可选：[Herdr](https://github.com/herdrdev/herdr)，用于在 Pane 中显示 Tick。

## 安装

`tickloop` 与 `tickhost` 总是一起安装和升级。daemon 拉起与自己同目录的 `tickhost`（找不到时用 PATH 中的），协议兼容时继续使用运行中的 Tickhost。

### Homebrew

```sh
brew install tickloopai/tap/tickloop
```

### 安装脚本

```sh
curl -fsSL https://tickloop.ai/install.sh | sh
```

脚本安装到 `~/.local/bin` 与 `~/.local/share/tickloop/versions/`，按当前系统与架构选择构建，固定发行标签并校验 SHA-256；`~/.local/bin` 不在 PATH 中时，会在登录 shell 的配置（zsh 为 `~/.zshrc`；bash 为 `~/.bashrc` 及 `~/.bash_profile` 或 `~/.profile`；fish 为 `conf.d`；其他为 `~/.profile`）里加入一次，并列出改动的文件；遇到其他来源的已有安装时报告冲突，不自动接管。脚本源码见 [scripts/install.sh](scripts/install.sh)。

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

Homebrew、安装脚本与 Cargo 启动器安装会在后台检查更新，不阻塞界面。更新不会停止运行中的 daemon、Agent 或 Tick。兼容的进程在安全点自动切换到新版本（Agent 在当前回合结束后），无需重启。

## 快速开始

1. 在项目目录运行 `tickloop`；要在别处启动，传入 `--cwd PATH`。
2. 首次启动会直接打开模型选择：选一个连接，还没有凭据时会要求输入 API Key 或登录，然后选模型；选定之前消息不会发送。之后可随时用 `/provider` 更换。API Key 保存在 `~/.tickloop/.env`（权限 `0600`），也可以使用 `DEEPSEEK_API_KEY` 等环境变量。
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

| 命令 | 用途 |
| --- | --- |
| `/help` | 按功能分组的命令（`/help advanced` 查看其余命令） |
| `/resume`、`/clear`、`/branch` | 打开已保存的 Chat、开始新 Chat，或从当前 Chat 分支 |
| `/plan` | 只读规划，批准计划后再开始工作 |
| `/goal`、`/loop` | 设定 Agent 持续推进的完成条件；按固定间隔或自定节奏重复执行一条指令 |
| `/stop`、`/monitor` | 打断当前轮、暂停 Trigger 直到下一条消息并停止子 Agent；显示或隐藏 Monitor |
| `/permissions`、`/settings` | 执行模式、Auto 审批与近期权限决定；模型、Thinking、输出风格等设置 |
| `/rewind`、`/diff` | 把代码和／或对话恢复到之前的提示；审阅文件变更 |
| `/compact`、`/details` | 压缩上下文；浏览与搜索详细会话记录（也可按 `ctrl+o`） |
| `/ticks`、`/artifacts` | 管理 Tick 包；登录并管理 Artifact |
| `/websearch` | 选择网络搜索服务 |
| `/exit` | 关闭当前界面，Agent 继续运行 |

命令行中，`tickloop -c` 继续本目录最近的 Chat，`tickloop -r [CHAT]` 恢复指定 Chat（省略时打开选择器），`--fork-session` 把恢复的 Chat 复制为新 Chat。`tickloop daemon stop` 停止 daemon 及全部运行中的 Agent；保存的历史保留，扩展 Tick 与 Terminal Tick 在 Tickhost 中继续运行，后台 Shell 随所属 Agent 结束。

### 按键

- `shift+tab` 在 `/settings` 中勾选的执行模式间切换（默认 Auto 与 Plan）。`enter` 发送；Agent 工作时消息排队，在下一个工具边界并入。`ctrl+enter` 中断并立即发送。
- `ctrl+j`（或终端能识别的 `alt+enter`／`shift+enter`）插入换行，`ctrl+g` 用外部编辑器编辑输入。文本输入没有 Vim 模式。
- 空闲时 `esc esc` 清空草稿，空输入时打开 Rewind。Agent 工作时（或有 active 的 Goal、自定节奏 `/loop` 时），`esc` 打断当前轮、暂停 Goal 并结束自定节奏 `/loop`。前台 `Bash` 运行满两秒后，`ctrl+b` 把它转入后台。
- 列表与菜单用 `j`／`k` 或 `ctrl+n`／`ctrl+p` 导航，`enter` 确认，`esc` 返回一层。
- 在 Monitor 中，`enter` 打开 Tick 或 Shell 的视图（Herdr 内为浮窗，否则全屏）。视图中 `esc` 隐藏视图、程序继续运行，`ctrl+b esc` 把 Esc 发给程序。在行上 3 秒内连按两次 `ctrl+x` 关闭 Tick、停止 Shell、移出该行、解除 Artifact 关联或删除定时任务；`s` 切换 Tick 的订阅频率，`p` 暂停或恢复订阅或定时任务，对已结束的 Tick 按 `enter` 重新打开并恢复订阅。

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

任何程序讲 Tick 协议就能成为扩展 Tick，见[协议规格](tick/protocol/SPECIFICATION.md)（[English](tick/protocol/SPECIFICATION.en.md)）、[Counter 示例](tick/examples/counter.py)与 Rust 的 Tick 侧 SDK [`tick/sdk`](tick/sdk)。

## 参与贡献

欢迎报告问题、提出想法和提交 Pull Request。构建、测试与评审流程见 [CONTRIBUTING.md](CONTRIBUTING.md)（英文）；领域词汇见 [CONTEXT.md](CONTEXT.md)，编码 Agent 的工作规则见 [AGENTS.md](AGENTS.md)。新的 Tick 包或对官方包的修改请提交到 [tickloopai/ticks](https://github.com/tickloopai/ticks)。

## 安全

请不要在公开 Issue 中报告漏洞，私下报告的方式见 [SECURITY.md](SECURITY.md)。

## 许可证

Tickloop 以 [MIT 许可证](LICENSE)发布。改编的第三方代码及其许可证见 [THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md)。
