# Tickloop

## 项目方向

Tickloop（tickloop.ai）是面向代码与办公的终端 AI Agent，以 Tick、产物、多模型支持和 Subagent 为核心能力；宿主不含任何领域的 Tick，领域 Tick 由 Tick 包提供。主命令 `tickloop` 启动 Agent；Tickhost 运行本机 Tick，原生 Herdr 负责 Workspace → Tab → Pane 与布局，Pane 只呈现 Tick 视图。首先支持 Herdr，tmux 尚未接入。

领域命名以 [CONTEXT.md](./CONTEXT.md) 为准；探索、设计和实现前读取领域文档约定（维护者本地 `docs/agents/domain.md`，存在时）及相关 ADR（维护者本地 `docs/adr/`，存在时；本文中的 `docs/…` 路径均同此，见“文档导航”）。不可将内部 Module 划分当成产品 Tick 类型。

## 核心行为

- Tick 的一次运行是独立 Tick Instance，身份不等于 Pane ID。本机 Tick（扩展 Tick、Terminal Tick、Shell Tick）全部由 Tickhost 运行，实例为 `hosted:` 与 `shell:`；Artifact Tick 在 Artifacts 服务中。人在 Pane 或终端里自己启动的程序不是 Tick。
- 每个 Chat 一个 Loop，持有执行状态；Goal 是可选的完成条件，Agent 在 Loop 中理解目标并调用工具。Tick Subscription 属于 Loop；打开、发现或读取不自动订阅。
- 扩展 Tick 讲 Tick 协议 3：stdin/stdout 按行 JSON-RPC，`tick/describe`、`tick/read`、`tick/call` 与 `tick/update` 通知，状态只有 `{seq, data, at?}`，审批绑定 Tick 自报的 `revision`。Agent 用 TickManage `open` 打开 Tick：裸程序名依次查配置根中已安装 Tick 包的程序（`tick-<包>`）、PATH；清单声明协议的包程序固定按协议，其他程序声明 `protocol: true` 时按协议，否则作为 Terminal Tick 在 PTY 中运行。
- Herdr Pane、Monitor 弹窗与普通终端只是附着视图，同一 Tick 可同时有多个视图；`open` 带 placement 时另建一个视图 Pane。关闭视图不结束 Tick，结束 Tick 用 TickManage `close` 或 Monitor。默认当前 Workspace，明确指定时可跨 Workspace；Herdr 外 Tick 照常运行，只是不能显示在 Pane 中。
- 订阅频率只有四档：及时（Shell 固定）、高频 1 分钟（扩展 Tick 与 Terminal Tick 的默认）、中频 10 分钟、低频 30 分钟。状态类 update 只给最新一份：扩展 Tick 的业务变化、Terminal Tick 到期时与上次送出不同的屏幕文字、Shell 的退出。Artifact 不接受 Tick Subscription，Artifact 关联向同一用户的每个关联 Agent 交付 @tickloop 留言，反馈没有执行任务状态（ADR 0028）。安静无模型调用、忙时合并和停止规则由现有 Loop runtime 持有。
- 订阅的已有内容只建立基线，后来加入和取消重启都有自己的起点。移动视图不改变实例身份，重启／重开需要重新选取，不静默改绑旧订阅。
- detach/attach 保留活进程。关闭 Agent Pane、`/exit` 或 View EOF 只离开界面，本机 daemon 中的 Agent 继续；Shell 随普通 Worker 退出结束，计划升级先交接所有权。执行所有者退出与历史装载／明确 Continue 分别处理，见 ADR 0013 与 ADR 0014（`docs/adr/0014-agent-work-binding-independent-of-view.md`）。修改安装升级、进程生命周期或版本回收前，读取 ADR 0029（`docs/adr/0029-graceful-local-runtime-upgrades.md`），以及维护者本地的平滑升级规格 `.scratch/graceful-upgrades/spec.md`（存在时）。
- 工具先解析目标和参数，再让同一份单次准备结果贯穿审批与执行。超时/取消发生在派发之后时可能是 OutcomeUnknown，修改操作不自动重发。Skill 不提供额外权限。

详细契约与操作入口见 Tickhost 与 Herdr 视图（`docs/design/native-herdr.md`） 与 ADR 0024（`docs/adr/0024-tickhost-runs-every-local-tick.md`）；维护者本地的实现规格与验收依据在 `.scratch/tickhost-unification/spec.md`，进度和证据在其 `issues/` 下各工单的记录（存在时）。设计记录不等于运行验收。

## 交互约束

Agent 的列表与菜单沿用 Vim 导航习惯，文本输入沿用 Claude Code 的普通编辑模式（见 ADR 0010（`docs/adr/0010-no-vim-mode-in-text-input.md`））；新增或调整输入、补全、选择器、权限菜单和任务视图时，统一遵守以下约定：

- 文本输入（含 Shell 输入）不提供 Vim 模式，编辑键沿用 Claude Code 普通模式；`Ctrl+G` 随时用外部编辑器编辑当前输入。
- 列表与菜单支持 `j`／`k` 和 `Ctrl+N`／`Ctrl+P` 导航，`Enter` 确认，`Esc` 返回或取消当前界面；文本输入中 `j`／`k` 保持文字输入，候选补全使用 `Ctrl+N`／`Ctrl+P` 导航。
- 菜单和选择器的选项不显示序号，也不提供看不见的数字快捷选择。Monitor、Subagents 面板等共享 `list_row` 的列表只用选中底色标记当前项，不加行首标记；菜单、选择器、权限与问题页继续用光标 `❯` 标记当前项（可同时带选中底色）。
- `Esc` 按当前上下文逐层处理：先关闭候选或当前弹层；关闭视图与停止任务分别操作。Agent 主输入界面没有候选或弹层消费时：对话上滚时先回到最新；运行中，或空闲而有 active 的 Goal 或自定节奏 `/loop` 且输入框为空时，`Esc` 打断当前一轮（如有）、暂停 active 的 Goal，并结束自定节奏 `/loop`（删除该任务），其他 Trigger 照常送达；其余空闲情况下 800 ms 内连按两次 `Esc` 清空草稿，空输入时打开 Rewind。
- Agent 输入框的 `Ctrl+C` 参照 Codex：先交给弹层与候选处理，输入框有文字时清空；运行中且输入框为空时等同 `Esc`；空闲且输入框为空时离开界面（Agents 总览中需 800 ms 内连按两次），Agent 在 Daemon 中继续。`/stop` 让 Loop 全部停下（打断、暂停 Goal、结束自定节奏 `/loop`、暂停接收 Trigger、停止全部后代），见 CONTEXT.md 的“停止”。
- Tick 视图的保留键是 `Esc`（隐藏视图）与 `Ctrl+B`（附着前缀，`Ctrl+B Esc` 把 Esc 发给程序），Tick 程序不应绑定这两个键：Tick 内返回上一层用 `Backspace`，文本输入同样是普通编辑，编辑中 `Ctrl+C` 取消编辑。
- Monitor 等列表上的破坏性行操作（移出或关闭 Tick、停止 Shell、解除 Artifact 关联、删除定时任务）统一为 3 秒内按两次 `Ctrl+X`，不弹确认框：首次在按键提示行显示红色 `Press ctrl+x again to …`，3 秒内在同一行再按才执行；到期、移动选中、按其他键或离开即取消。
- 按键只作用于当前可见且聚焦的界面；权限、问题和工作流菜单与输入编辑器分别处理按键，隐藏界面不消费输入。界面提示与实际按键行为保持一致。

AI 只能在既有授权范围内执行。读取与写入授权分别判断；新增接入不扩大已有权限。持续执行依赖本机运行环境。

新增或修改任何模型工具时，同时确定它在 Auto 模式下的处理：只读工具标为 ReadOnly；有副作用的工具对照 Claude Code 的 auto-mode 默认政策，在 `agent/config/src/auto_policy/`（`defaults.json` 与 README 的映射、“不适用”清单与计数）中给出映射，并补测试，确保正常的用户请求在 Auto 模式下不被误拒、越权操作仍被拦截。

## 结构与接入边界

| 路径 | 内容 |
| --- | --- |
| `agent/tickloop/` | 可执行文件 `tickloop`：分派 Console、`update`、`daemon run`／`daemon stop`、`worker`、`monitor` 与 `tick`（Tick 包的 install／uninstall／login／logout／list，安装与登录经 `agent/packages`）；端到端测试（`tests/all/main.rs`）与供测试调用的 `cli_fixture` 程序 |
| `agent/console/` | Agent Console（terminal、agent_console、Monitor 行，含 `/ticks` 面板），只经 AgentAccess 访问 Agent，不依赖 runtime、daemon 与 tickhost/host（三者只用于它的测试）；Tick 包经 `agent/packages` 管理 |
| `agent/packages/` | Tick 市场的读取与下载（https，市场在本机时才接受本机 http），`/ticks` 与 `tickloop tick` 共用的安装、更新、卸载、登录与退出登录（OAuth 经 tick/oauth，按 base 的 HTTP 策略建客户端）；依赖 base、tick/packages 与 tick/oauth |
| `agent/client/` | AgentAccess 客户端：Unix socket 传输、断线恢复、查找或启动本机 daemon（运行目录只由配置根推导，一个配置根一个 daemon）；只依赖 protocol 与 tick/packages（配置根） |
| `agent/daemon/` | daemon 与 worker 进程：AgentAccess 服务端、嵌入式 Agent、worker 监督与 hosted 控制通道；除 tickhost/connect 外还以库依赖 tickhost/host |
| `agent/runtime/` | Agent runtime：Loop、Subagent、Transcript 存储与装载（TranscriptStore、JSONL）、Tick 订阅流、宿主工具、hosted 与 MCP；除 tickhost/connect 外还以库依赖 tickhost/host（运行目录规则、宿主类型与 Shell 提交和交接） |
| `agent/transcript/` | Transcript 记录格式：`Line` 及各 entry、行类型头部识别与显示投影 `display`；只依赖 protocol 与 serde 一类基础库，供 runtime 与只读写记录格式、不需要 runtime 的使用方共用 |
| `agent/base/` | Agent 通用底层：私有文件、HTTP 策略、图片与输出样式 |
| `agent/protocol/` | Agent 全部线上类型与 AgentAccess 契约（ADR 0022），只依赖 tick/protocol 与 serde 一类基础库 |
| `agent/model/` | 模型客户端、Provider 登录与 judgment，依赖 base、protocol |
| `agent/tools/` | 工具、权限规则与文件变更记录，依赖 base、protocol |
| `agent/cron/` | 定时任务计时：调用方给定时区下的 5 段 cron、`/loop` 间隔、抖动与任务 id；不依赖其他本仓库 crate，被 config 与 runtime 依赖 |
| `agent/config/` | 用户设置与 Skill registry（含内置 Skill 正文；用户、项目与已安装 Tick 包的 Skill），依赖 model、tools、protocol、base、cron 与 tick/packages |
| `agent/artifacts/` | Tickloop Artifacts 与 Accounts 客户端（设备登录、凭据、目录、反馈投递、Slides 图片上传），依赖 protocol、base 与 tick/oauth |
| `agent/updater/` | 按版本目录安装、更新检查与固定运行副本，依赖 base 与 tickhost/connect |
| `agent/test-support/` | Agent 各 crate 测试共用的代码：ScriptedModel、内存与注入故障的 Transcript Store、无状态宿主的参考发布者与内存 Event Stream（含契约测试）、PTY 屏幕、传输限额与假 HTTP 服务；依赖 config、model、protocol、runtime、transcript 与 tick/protocol |
| `tickhost/host/` | 可执行文件 `tickhost` 与同名库（crate `tickloop-tickhost`）：daemon 拉起的隐藏宿主服务 `tickhost run`（协议、终端与 Shell 适配器，多视图附着）、隐藏的 launcher `tickhost launch`，以及公开的 `tickhost stop`（结束当前配置根宿主中的全部 Tick） |
| `tickhost/connect/` | Tickhost 业务通道与客户端、每实例 TickSession、launch 契约、打开时的程序解析（已安装 Tick 包、PATH）、Herdr socket 与视图 Pane 布局；不依赖 Agent |
| `tick/ids/` | Tickloop 自己的 ID：唯一的 UUID v7 生成入口、每类对象一个 `Id<Kind>` 类型、与 Tickloop Cloud 逐字节一致的 22 位 base62（含共用测试向量）、模型文本出口与参数解析的 ID 上下文（`IdContext`、`ModelRef`）及统一报错；只依赖 `uuid` 与 `serde`，tickhost 与 Agent 共用 |
| `tick/protocol/`、`tick/sdk/` | Tick 标准（版本、方法、帧与限额），以及 Tick 侧协议实现 |
| `tick/oauth-test-server/` | 仅供测试的假 OAuth 2.0 授权服务器（127.0.0.1），Tick 与 Agent 的登录测试共用；`make build`／`install`／`package` 不包含它 |
| `tick/oauth/` | Tick 包登录的标准 OAuth 2.0 客户端：RFC 8414 发现、7591 注册、8628 设备码轮询、6749 刷新、7009 撤销与 https／本机地址规则；带 OAuth `error` 的 400 与任何 401 算拒绝，其余非 2xx 退避重试或报网络错误；HTTP 客户端由调用方提供（Agent 用 base 的策略，包程序用自己的），不依赖 Agent crate |
| `tick/packages/` | Tick 包：解析校验 `tick-package.json`（含 `none`／`api_key`／`oauth2` 三种 `auth`）与 Tick 市场的 `marketplace.json`，在配置根的 `ticks/` 下安装（包目录或 `.tar.gz`，暂存后原子替换）、卸载、列出，并维护 `installed.json`；在 `credentials/<包>/` 下保存凭据（加锁、原子写）、按文件判断登录状态，并给出包程序启动时的凭据环境（`TICKLOOP_OAUTH_TOKEN_FILE` 或密钥变量）与程序找令牌文件的规则；`agent/packages` 调用它并负责下载与授权，它不依赖 Agent crate |
| `ui/` | 共享终端呈现、输入和格式规则 |
| `tick/examples/` | 项目外协议程序的可运行示例 |
| `distribution/cargo/` | crates.io 安装启动器 `tickloop`：安装时下载、校验并嵌入官方预编译包，独立于主 workspace |
| `vendor/` | 通过 Cargo patch 接入的 crossterm 副本 |
| `.scratch/` | 维护者本地工作区，不纳入 Git：规格、工单、复现脚本与验证记录；贡献者的仓库中没有它 |

Agent 的 HostToolProvider 与 TickStreamPort 是主要接入 Seam：runtime 只为 `hosted` 与 `shell` 注册 Tick 适配器，各提供一个经 Tickhost 观察实例的 TickStreamPort；协议、终端与 Shell 的执行在 Tickhost 中。Artifact 不是 Tick 适配器，@tickloop 反馈经 Artifact 关联进入 Loop。Shell 的审批留在 Worker，Tickhost 只执行一次性起进程授权绑定的请求。Loop 不持有 Herdr 布局状态，数据 Source 留在 Tick 包中。通用 TickCall 读取目录再调用，不为每个实例生成模型工具；见 ADR 0007、0008、0019、0024。

系统级 Skill 仍名为 tickloop，支持模型渐进披露与用户显式调用；其指令必须匹配实际工具和权限。各数据来源的 argv 写在 Tick 包自带的 Skill 中（`<包>:usage`），系统 Skill 不写死。最小子 Agent 由 `Subagent` 真实委派：前台等待最终报告，后台立即返回并主动推送完成通知；消息自动进入接收 Loop，空闲接收方开始新回合，模型无需查询收件箱。后续输入保持原授权上限；Subagent 可自行派生，最多三层，全部后代仍属于同一家族。根可停止后代子树，成员可停止直接孩子子树，用户在某个 Agent 上 `/stop` 同时停止它的全部后代；完成通知照 Claude Code 规则送达（见 `docs/design/agent.md`）；家族内一对一消息和按需共享 Task 已开放，Task 工具由 Settings 的唯一开关控制，默认关闭，主／子 Agent 一致，不由后台或创建成员触发；单人与团队共用任务列表，开启且有数据才显示，关闭立即隐藏并保留数据。修改委派、协作任务或其界面前读取 ADR 0026（`docs/adr/0026-one-subagent-family-and-shared-tasks.md`），以及维护者本地的协作规格 `.scratch/subagent-collaboration/spec.md`（存在时）。跨家族通信、广播与自动任务领取不提供。通用 MCP 服务器、ACP 与 Hooks 尚未接入。

本仓库是领域无关的宿主：不认识任何 Tick 种类、领域概念或具体包程序名。官方 Tick 包（程序、包清单与 Skill）及其打包、发布在公开仓库 [tickloopai/ticks](https://github.com/tickloopai/ticks)（同时是 Tick 市场，包源码在 `packages/<包>/`，标签 `<包>-v<版本>`），本仓库只保留包机制。宿主接入的验收不需要任何真实数据源账户。

## 验证与工单

使用当前 Rust stable（edition 2024）。日常与提交 PR 前要求 `make check-workspace`（与 CI 相同：`cargo fmt --all --check`、Cargo 启动器的 fmt 检查、`cargo clippy --locked --workspace --all-targets -- -D warnings`，以及 `make test-workspace` 的 cargo-nextest 全量测试，配置见 `.config/nextest.toml`；安装：`cargo install --locked cargo-nextest`）；发布前另跑 `make test`，它还包含需要联网下载对应发行的 Cargo 启动器测试。每个 crate 至多一个集成测试二进制 `all`（`tests/all.rs` 与 `tests/all/*.rs`；`agent/tickloop` 为 `tests/all/main.rs`，另有供测试调用的 `cli_fixture` 程序），测试放在被测对象所在的 crate，只有需要完整可执行文件或 cli_fixture 的留在 `agent/tickloop/tests`。优先从 Agent 工具/权限、Tick Subscription→Loop 测试；协议使用真实子进程/PTY，宿主使用可控 Adapter 与隔离 Herdr。只清理测试自己创建的资源。分别记录本地自动检查、真实宿主、真实模型和用户操作证据。

公开协作使用 GitHub Issues 与 Pull Request，CI（`.github/workflows/ci.yml`）运行 `make check-workspace`、发布脚本测试与 Linux musl 目标的 clippy；它不随 push、PR 或发布触发，只由维护者在 Actions 中手动启动；提交 PR 前与发布前在本地跑齐检查。维护者的本地工单按功能放在 `.scratch/<feature>/issues/`（不纳入 Git），实现前读完整工单、父规格和依赖状态。规范见工单约定（`docs/agents/issue-tracker.md`）与状态映射（`docs/agents/triage-labels.md`）。Status 是分流状态，Progress 才是实施进度，完成必须有验收证据。

脚本产生的日志、results、evidence、transcript、缓存必须被 .gitignore 排除；规格、工单与验证记录只留在维护者本地的 `.scratch/`。Git 操作仍受用户明确授权约束，避免将 CRLF 运行输出纳入源文件变更。

## 文档导航

项目的 Agent 指令只写在 AGENTS.md，不建 CLAUDE.md；CONTEXT.md 只放术语定义；README 只写面向用户的用法。

设计记录、ADR、Agent 中文手册与项目 Skill（`docs/`、`agent/tickloop/README.md`、`.tickloop/skills/`）是维护者本地文档，不纳入 Git；本文提到的 `docs/…` 路径都指维护者本地 `docs/…`（存在时），贡献者的检出中没有它们，以本文、CONTEXT.md、README 与源码为准。

- Agent 行为、UI 与模型/权限：Agent 中文手册（`agent/tickloop/README.md`）；Agent 设计（`docs/design/agent.md`：Loop、Trigger、Goal、定时任务、Esc／`/stop`、Subagent、权限与 daemon／worker 生命周期，Loop 的决定见 ADR 0030）；Agent 界面设计（`docs/design/agent-ui.md`：Monitor 与界面交互）；模型与 Provider 接入见项目 Skill `.tickloop/skills/model-integration/`。
- Tick 标准与双向操作：Tickhost 与 Herdr 视图（`docs/design/native-herdr.md`：打开、视图、订阅与生命周期）、Tick 类型与接入（`docs/design/tick.md`）、当前架构（`docs/design/architecture.md`）、Tick 包（`docs/design/tick-packages.md`）、ADR 0007/0008/0019/0024，以及维护者本地的本机 Tick 统一规格 `.scratch/tickhost-unification/spec.md`。
- 图片输入、模型图片能力与图片存储：图片输入设计记录（`docs/design/image-input.md`），本期只做图片，不含 PDF。
- UI 样式、Glyph、Theme：UI 样式、字形与宿主边界（`docs/design/ui-theme.md`）；Role 表、主题与字形映射见 `docs/design/agent-ui.md`。
- Artifacts 客户端：`docs/design/ai-artifacts.md`；安装、更新与发布：`docs/design/auto-updates.md`、`docs/design/releases.md`。
- 历史研究与参考源码仅作证据，不覆盖当前规格；Herdr 集成以已安装版本的 CLI/schema 为准，不自动升级或重启用户服务。

子 Agent 的 Bash 和直接 `!` Shell 沿冻结父权限、当前收紧规则和单次审批执行；cwd 不是操作系统沙箱。同一家族共享自己的私有临时结果／scratchpad 目录，其他家族不自动获权，符号链接越界也不自动放行。
