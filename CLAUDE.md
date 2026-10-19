# Tickloop

## 项目方向

Tickloop（tickloop.ai）是面向代码与办公的终端 AI Agent，以 Tick、产物、多模型支持和 Subagent 为核心能力，并提供金融等场景的 example Tick。主命令 `tickloop` 启动 Agent；Tickhost 运行本机 Tick，原生 Herdr 负责 Workspace → Tab → Pane 与布局，Pane 只呈现 Tick 视图。自研工作台及其兼容层已移除。首先支持 Herdr，tmux 尚未接入。

领域命名以 [CONTEXT.md](./CONTEXT.md) 为准；探索、设计和实现前读取领域文档约定（维护者本地 `docs/agents/domain.md`，存在时）及相关 ADR（维护者本地 `docs/adr/`，存在时；本文中的 `docs/…` 路径均同此，见“文档导航”）。不可将内部 Module 划分当成产品 Tick 类型。

## 核心行为

- Tick 的一次运行是独立 Tick Instance，身份不等于 Pane ID。本机 Tick（扩展 Tick、Terminal Tick、Shell Tick）全部由 Tickhost 运行，实例为 `hosted:` 与 `shell:`；Artifact Tick 在 Artifacts 服务中。人在 Pane 或终端里自己启动的程序不是 Tick。
- Loop 持有目标与执行状态，Agent 在其中理解目标并调用工具。Tick Subscription 属于 Loop；打开、发现或读取不自动订阅。
- 扩展 Tick 讲 Tick 协议 3：stdin/stdout 按行 JSON-RPC，`tick/describe`、`tick/read`、`tick/call` 与 `tick/update` 通知，状态只有 `{seq, data, at?}`，审批绑定 Tick 自报的 `revision`。Agent 用 TickManage `open` 打开 Tick：裸程序名依次查 Agent 旁的自带程序、配置根中已安装 Tick 包的程序（`tick-<包>`）、PATH；自带 `tick-reader` 与清单声明协议的包程序固定按协议，其他程序声明 `protocol: true` 时按协议，否则作为 Terminal Tick 在 PTY 中运行。
- Herdr Pane、Monitor 弹窗与普通终端只是附着视图，同一 Tick 可同时有多个视图；`open` 带 placement 时另建一个视图 Pane。关闭视图不结束 Tick，结束 Tick 用 TickManage `close` 或 Monitor。默认当前 Workspace，明确指定时可跨 Workspace；Herdr 外 Tick 照常运行，只是不能显示在 Pane 中。
- 订阅频率只有四档：及时（Shell 固定）、高频 1 分钟（扩展 Tick 与 Terminal Tick 的默认）、中频 10 分钟、低频 30 分钟。状态类 update 只给最新一份：扩展 Tick 的业务变化、Terminal Tick 到期时与上次送出不同的屏幕文字、Shell 的退出。Artifact 不接受 Tick Subscription，Artifact 关联向同一用户的每个关联 Agent 交付 @tickloop 留言，反馈没有执行任务状态（ADR 0028）。安静无模型调用、忙时合并和停止规则由现有 Loop runtime 持有。
- 订阅的已有内容只建立基线，后来加入和取消重启都有自己的起点。移动视图不改变实例身份，重启／重开需要重新选取，不静默改绑旧订阅。
- detach/attach 保留活进程。关闭 Agent Pane、`/exit` 或 View EOF 只离开界面，本机 daemon 中的 Agent 继续；Shell 随普通 Worker 退出结束，计划升级先交接所有权。执行所有者退出与历史装载／明确 Continue 分别处理，见 ADR 0013、0014。修改安装升级、进程生命周期或版本回收前，读取 ADR 0029（`docs/adr/0029-graceful-local-runtime-upgrades.md`），以及维护者本地的平滑升级规格 `.scratch/graceful-upgrades/spec.md`（存在时）。
- 工具先解析目标和参数，再让同一份单次准备结果贯穿审批与执行。超时/取消发生在派发之后时可能是 OutcomeUnknown，修改操作不自动重发。Skill 不提供额外权限。

详细契约与操作入口见 Tickhost 与 Herdr 视图（`docs/design/native-herdr.md`） 与 ADR 0024（`docs/adr/0024-tickhost-runs-every-local-tick.md`）；维护者本地的实现规格与验收依据在 `.scratch/tickhost-unification/spec.md`，进度和证据在其 `issues/` 下各工单的记录（存在时）。设计记录不等于运行验收。

## 交互约束

Agent 的列表与菜单沿用 Vim 导航习惯，文本输入沿用 Claude Code 的普通编辑模式（见 ADR 0010（`docs/adr/0010-no-vim-mode-in-text-input.md`））；新增或调整输入、补全、选择器、权限菜单和任务视图时，统一遵守以下约定：

- 文本输入（含 Shell 输入）不提供 Vim 模式，编辑键沿用 Claude Code 普通模式；`Ctrl+G` 随时用外部编辑器编辑当前输入。
- 列表与菜单支持 `j`／`k` 和 `Ctrl+N`／`Ctrl+P` 导航，`Enter` 确认，`Esc` 返回或取消当前界面；文本输入中 `j`／`k` 保持文字输入，候选补全使用 `Ctrl+N`／`Ctrl+P` 导航。
- 菜单和选择器的选项不显示序号，也不提供看不见的数字快捷选择。Monitor、Subagents 面板等共享 `list_row` 的列表只用选中底色标记当前项，不加行首标记；菜单、选择器、权限与问题页继续用光标 `❯` 标记当前项（可同时带选中底色）。
- `Esc` 按当前上下文逐层处理：先关闭候选或当前弹层；关闭视图与停止任务分别操作。
- Tick 视图的保留键是 `Esc`（隐藏视图）与 `Ctrl+B`（附着前缀，`Ctrl+B Esc` 把 Esc 发给程序），自带 Tick 不绑定这两个键：Tick 内返回上一层用 `Backspace`，文本输入同样是普通编辑，编辑中 `Ctrl+C` 取消编辑。
- 按键只作用于当前可见且聚焦的界面；权限、问题和工作流菜单与输入编辑器分别处理按键，隐藏界面不消费输入。界面提示与实际按键行为保持一致。

AI 只能在既有授权范围与投资风险约束内执行。行情、账户和交易授权分别判断；新增接入不扩大交易权限。持续执行依赖本机运行环境。

## 结构与接入边界

| 路径 | 内容 |
| --- | --- |
| `agent/tickloop/` | 可执行文件 `tickloop`：分派 Console、`daemon run`／`daemon stop`、`worker`、`monitor` 与 `tick`（Tick 包的 install／uninstall／login／logout／list，安装与登录经 `agent/packages`）；端到端测试与 cli_fixture |
| `agent/console/` | Agent Console（terminal、agent_console、Monitor 行，含 `/ticks` 面板），只经 AgentAccess 访问 Agent，不依赖 runtime 与 daemon；Tick 包经 `agent/packages` 管理 |
| `agent/packages/` | Tick 市场的读取与下载（https，市场在本机时才接受本机 http），`/ticks` 与 `tickloop tick` 共用的安装、更新、卸载、登录与退出登录（OAuth 经 tick/oauth，按 base 的 HTTP 策略建客户端）；依赖 base、tick/packages 与 tick/oauth |
| `agent/client/` | AgentAccess 客户端：Unix socket 传输、断线恢复、查找或启动本机 daemon；只依赖 protocol |
| `agent/daemon/` | daemon 与 worker 进程：AgentAccess 服务端、嵌入式 Agent、worker 监督与 hosted 控制通道 |
| `agent/runtime/` | Agent runtime：Loop、Subagent、Transcript 存储与装载（TranscriptStore、JSONL）、Tick 订阅流、宿主工具、hosted 与 MCP |
| `agent/transcript/` | Transcript 记录格式：`Line` 及各 entry、行类型头部识别与显示投影 `display`；只依赖 protocol 与 serde 一类基础库，供 runtime 与只读写记录格式、不需要 runtime 的使用方共用 |
| `agent/base/` | Agent 通用底层：私有文件、HTTP 策略、图片与输出样式 |
| `agent/protocol/` | Agent 全部线上类型与 AgentAccess 契约（ADR 0022），只依赖 tick/protocol 与 serde 一类基础库 |
| `agent/model/` | 模型客户端、Provider 登录与 judgment，依赖 base、protocol |
| `agent/tools/` | 工具、权限规则与文件变更记录，依赖 base、protocol |
| `agent/config/` | 用户设置与 Skill registry（含内置 Skill 正文；用户、项目与已安装 Tick 包的 Skill），依赖 model、tools、protocol、base 与 tick/packages |
| `agent/artifacts/` | Artifact Service 客户端（登录、凭据、目录），依赖 protocol、base |
| `agent/updater/` | 按版本目录安装、更新检查与固定运行副本，依赖 base 与 tickhost/connect |
| `agent/recurrence/` | Loop Trigger 的墙钟重复规则：解析、规范文本与下次运行时间，不依赖其他 crate |
| `agent/test-support/` | Agent 各 crate 测试共用的代码：ScriptedModel、内存与注入故障的 Transcript Store、无状态宿主的参考发布者与内存 Event Stream（含契约测试）、PTY 屏幕、传输限额与假 HTTP 服务；依赖 model、protocol、runtime 与 transcript |
| `tickhost/host/` | 可执行文件 `tickhost`：daemon 拉起的宿主服务 `tickhost run`（协议、终端与 Shell 适配器，多视图附着）、宿主内部的隐藏 launcher `tickhost launch` 与 `tickhost stop` |
| `tickhost/connect/` | Tickhost 业务通道与客户端、每实例 TickSession、launch 契约、打开时的程序解析（自带程序、已安装 Tick 包、PATH）、Herdr socket 与视图 Pane 布局；不依赖 Agent、金融 Tick 或旧工作台 |
| `tick/protocol/`、`tick/sdk/` | Tick 标准（版本、方法、帧与限额），以及 Tick 侧协议实现 |
| `tick/provider/` | provider 接口（QuoteApi、TradeApi、NewsApi、OptionsApi、CommunityApi），由 [tickloopai/ticks](https://github.com/tickloopai/ticks) 中的 Tick 包与测试 mock 实现 |
| `tick/market/` | 金融 Tick 本体，只依赖 provider 接口、sdk 与 ui |
| `tick/command/` | Tick Command 的共享入口：全局显示参数、配置、locale 检查、`describe` 与 `run_tick` |
| `tick/reader/` | 可执行文件 `tick-reader`：Reader，随 Tickloop 安装 |
| `tick/test-support/` | 测试用模拟数据、仅供测试的 `tick-mock` 与假 OAuth 授权服务器（Tick 与 Agent 的登录测试共用）；`make build`／`install`／`package` 不包含它 |
| `tick/oauth/` | Tick 包登录的标准 OAuth 2.0 客户端：RFC 8414 发现、7591 注册、8628 设备码轮询、6749 刷新、7009 撤销与 https／本机地址规则；带 OAuth `error` 的 400 与任何 401 算拒绝，其余非 2xx 退避重试或报网络错误；HTTP 客户端由调用方提供（Agent 用 base 的策略，`tick-longbridge` 用自己的），不依赖 Agent crate |
| `tick/packages/` | Tick 包：解析校验 `tick-package.json`（含 `none`／`api_key`／`oauth2` 三种 `auth`）与 Tick 市场的 `marketplace.json`，在配置根的 `ticks/` 下安装（包目录或 `.tar.gz`，暂存后原子替换）、卸载、列出，并维护 `installed.json`；在 `credentials/<包>/` 下保存凭据（加锁、原子写）、按文件判断登录状态，并给出包程序启动时的凭据环境（`TICKLOOP_OAUTH_TOKEN_FILE` 或密钥变量）与程序找令牌文件的规则；`agent/packages` 调用它并负责下载与授权，它不依赖 Agent crate |
| `ui/` | 共享终端呈现、输入和格式规则 |
| `tick/examples/` | 项目外协议程序的可运行示例 |
| `distribution/cargo/` | crates.io 安装启动器 `tickloop`：安装时下载、校验并嵌入官方预编译包，独立于主 workspace |
| `vendor/` | 通过 Cargo patch 接入的 crossterm 副本 |
| `.scratch/` | 维护者本地工作区，不纳入 Git：规格、工单、复现脚本与验证记录；贡献者的仓库中没有它 |

Agent 的 HostToolProvider 与 TickStreamPort 是主要接入 Seam：Worker 侧只有 Tickhost 客户端（`hosted:` 与 `shell:`）和 Artifact 两个 Tick 适配器，协议、终端与 Shell 适配器在 Tickhost 中。Shell 的审批留在 Worker，Tickhost 只执行一次性起进程授权绑定的请求。Loop 不持有 Herdr 布局状态，金融 Source 留在示例 Tick 中。通用 TickCall 读取目录再调用，不为每个实例生成模型工具；见 ADR 0007、0008、0019、0024。

系统级 Skill 仍名为 tickloop，支持模型渐进披露与用户显式调用；其指令必须匹配实际工具和权限。各数据来源的 argv 写在 Tick 包自带的 Skill 中（`<包>:usage`），系统 Skill 不写死。最小子 Agent 由 `Subagent` 真实委派：前台等待最终报告，后台立即返回并主动推送完成通知；消息自动进入接收 Loop，空闲接收方开始新回合，模型无需查询收件箱。后续输入保持原授权上限；Subagent 可自行派生，最多三层，全部后代仍属于同一家族。根可停止后代子树，成员可停止直接孩子子树；家族内一对一消息和按需共享 Task 已开放，Task 工具由 Settings 的唯一开关控制，默认关闭，主／子 Agent 一致，不由后台或创建成员触发；单人与团队共用任务列表，开启且有数据才显示，关闭立即隐藏并保留数据。修改委派、协作任务或其界面前读取 ADR 0026（`docs/adr/0026-one-subagent-family-and-shared-tasks.md`），以及维护者本地的协作规格 `.scratch/subagent-collaboration/spec.md`（存在时）。跨家族通信、广播、自动任务领取及整家族 stop 不提供。跨视图财报导航、自动研究草稿、MCP、ACP、Hooks 和策略建模不在这次迁移内。

金融接入继续使用 Longbridge、独立 Futu OpenD 和 Reader 原文来源。Tick 包 `longbridge`、`futu`（`tick-longbridge`、`tick-futu`、包清单与 Skill）及其打包、发布在公开仓库 [tickloopai/ticks](https://github.com/tickloopai/ticks)（同时是 Tick 市场，包源码在 `packages/<包>/`，标签 `<包>-v<版本>`），本仓库只保留包机制与 Reader。新增能力按具体 API、市场、账户权限及连接条件核验；无需真实金融账户即可验收本次宿主接入。

## 验证与工单

使用当前 Rust stable（edition 2024）。要求 `cargo fmt --all --check`、`make test`（cargo-nextest 全量运行，配置见 `.config/nextest.toml`；安装：`cargo install --locked cargo-nextest`）、`cargo clippy --locked --workspace --all-targets -- -D warnings`。每个 crate 只有一个集成测试二进制（`tests/all.rs` 与 `tests/all/*.rs`），测试放在被测对象所在的 crate，只有需要完整可执行文件或 cli_fixture 的留在 `agent/tickloop/tests`。优先从 Agent 工具/权限、Tick Subscription→Loop 测试；协议使用真实子进程/PTY，宿主使用可控 Adapter 与隔离 Herdr。只清理测试自己创建的资源。分别记录本地自动检查、真实宿主、真实模型和用户操作证据。

公开协作使用 GitHub Issues 与 Pull Request，CI（`.github/workflows/ci.yml`）运行上述三项检查。维护者的本地工单按功能放在 `.scratch/<feature>/issues/`（不纳入 Git），实现前读完整工单、父规格和依赖状态。规范见工单约定（`docs/agents/issue-tracker.md`）与状态映射（`docs/agents/triage-labels.md`）。Status 是分流状态，Progress 才是实施进度，完成必须有验收证据。

脚本产生的日志、results、evidence、transcript、缓存必须被 .gitignore 排除；规格、脚本和结果说明可随仓库保留。本机 core.safecrlf=true；Git 操作仍受用户明确授权约束，避免将 CRLF 运行输出纳入源文件变更。

## 文档导航

设计记录、ADR、Agent 中文手册与项目 Skill（`docs/`、`agent/tickloop/README.md`、`.tickloop/skills/`）是维护者本地文档，不纳入 Git；本文提到的 `docs/…` 路径都指维护者本地 `docs/…`（存在时），贡献者的检出中没有它们，以本文、CONTEXT.md、README 与源码为准。

- Agent UI 与模型/权限：Agent 中文手册（`agent/tickloop/README.md`）、Agent 设计、Agent 视觉规格和 Loop 统一模型（`docs/design/`）。
- Tick 标准与双向操作：Tickhost 与 Herdr 视图、Tick 设计、ADR 0007/0008/0019/0024，以及维护者本地的本机 Tick 统一规格 `.scratch/tickhost-unification/spec.md`。
- 图片输入、模型图片能力与图片存储：图片输入设计记录（`docs/design/image-input.md`），本期只做图片，不含 PDF。
- UI 样式、Glyph、Theme：界面呈现设计与 UI 样式接入规范。
- 历史研究与参考源码仅作证据，不覆盖当前规格；Herdr 集成以已安装版本的 CLI/schema 为准，不自动升级或重启用户服务。

子 Agent 的 Bash 和直接 `!` Shell 沿冻结父权限、当前收紧规则和单次审批执行；cwd 不是操作系统沙箱。同一家族共享自己的私有临时结果／scratchpad 目录，其他家族不自动获权，符号链接越界也不自动放行。
