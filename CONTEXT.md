# tickloop.ai

Tickloop 的产品术语表。本文只给术语定义与命名约定；行为契约的公开摘要在 [AGENTS.md](./AGENTS.md)，详细行为在维护者本地的设计记录（`docs/design/`，不纳入 Git），两者与源码冲突时以源码为准。产品与组件名在中英文文档和界面中一律使用英文原名。

## Language

### Tick

**Tick**：
Agent 可以描述、读取、调用并在变化时收到 Tick Update 的一个运行中对象。Tick 只有四种类型（见“Tick 类型”）；人在 Herdr Pane 或其他终端里自己启动的程序不是 Tick。

**Tick Instance**：
一个 Tick 的一次独立运行，持有自己的业务身份与运行状态，身份写作 `<类型>:<id>`（`hosted:`、`shell:`、`artifact:`）。身份独立于呈现它的 Pane 或其他视图；关闭后重开是一个新实例。
_Avoid_：用 Pane ID 指代 Tick

**Tick 标准（Tick Standard）**：
所有 Tick 类型共享的统一契约：实例身份，describe、read、call、update 四种能力（每种类型只提供自己能提供的），以及更新经 Tick Subscription 或 Artifact 关联交给 Loop 的规则。
_Avoid_：把 Tick 标准当作线上协议的名称

**Tick 协议（Tick Protocol）**：
程序经 stdin/stdout 按行 JSON-RPC 向宿主提供状态、工具和变化的线上约定，当前为协议 3；实现它的程序是扩展 Tick。规范见 `tick/protocol/SPECIFICATION.md`。

**Tick 适配器（Tick Adapter）**：
宿主为一种实例前缀注册、向 Loop 提供该类型 Tick Update 流（TickStreamPort）的部件。当前只有 `hosted` 与 `shell` 两个，都经 Tickhost 观察实例；Artifact 没有 Tick 适配器，它的反馈经 Artifact 关联进入 Loop。适配器不授予权限，也不增加模型工具。

**Tick Update**：
Tick Instance 产生的一次带来源与时间的数据或状态变化，是 Loop 可以据此继续执行的新信息。状态类更新只保留最新一份（扩展 Tick 的业务变化、Terminal Tick 的屏幕文字变化与退出、Shell Tick 的退出）；Artifact 反馈逐条送达，不合并。
_Avoid_：事件（泛指时）、通知

**Tick 工具**：
Agent 操作 Tick 的模型工具：TickDescribe 查询能力与工具目录，TickRead 读取当前状态，TickCall 调用扩展 Tick 声明的 Tick Tool，TickManage 管理实例、视图、终端输入与订阅。Shell 与 Artifact 沿用各自的原生工具（Bash／Read／TaskStop；ArtifactRead／ArtifactEdit／ArtifactComment／ArtifactUpload）。TickUpdate 与 TickSubscription 是内容流中的事件和关系名称，不是模型工具。

**Tick Tool**：
扩展 Tick 在自身描述中声明、供 Agent 经 TickCall 调用的能力，分查询类（返回数据）与动作类（改变该实例的内容或呈现）。Tick Tool 不改变 Layout，也不带来任何额外授权。
_Avoid_：Action（只指动作类的一种）

**Tick Subscription（Tick 订阅）**：
Chat 的 Loop 与一个 Tick Instance 之间明确建立的关联，是 Loop 的一种 Trigger。频率只有四档：及时、高频（1 分钟）、中频（10 分钟）、低频（30 分钟）；及时只用于 Shell Tick。打开、发现或读取 Tick 都不等于订阅；Artifact 不接受 Tick Subscription。
_Avoid_：监听、Watch、轮询

**Tickhost（Tick 宿主服务）**：
运行本机全部 Tick（扩展 Tick、Terminal Tick、Shell Tick）并持有其终端与屏幕的本机服务，由 Daemon 按需拉起，每个配置根一个；Herdr Pane、Monitor 弹窗与普通终端都只是附着在它上面的视图。程序、进程与命令统一写作 `tickhost`。
_Avoid_：Tick host、tick-host、tick_host

**视图（Tick View）**：
附着到一个 Tick Instance 的显示与输入界面；同一 Tick 可以同时有多个视图，关闭视图不结束 Tick。Esc（隐藏视图）与 Ctrl+B（附着前缀）属于视图。

**Tick 所有权（Tick Ownership）**：
一个持久 Chat 对它打开的 Tick 的生存期责任，由配置根与 Chat 身份共同确定；子 Agent 的 Chat 独立拥有自己的 Tick。

### Tick 类型

**扩展 Tick（Extension Tick）**：
实现 Tick 协议、由 Agent 用 TickManage `open` 打开并在 Tickhost 中运行的程序，提供结构化状态、自己的 Tick Tool 与业务更新。
_Avoid_：Tick Command、协议 Tick

**示例 Tick（Example Tick）**：
随项目提供、演示扩展 Tick 用法的程序（`tick/examples/counter.py`）。

**Terminal Tick**：
Agent 让 Tickhost 在终端（PTY）中运行的、不讲 Tick 协议的交互式程序，例如 htop 或 REPL；读取得到屏幕文字，输入经 TickManage。
_Avoid_：终端观察（Terminal Observation，已取消）

**Shell Tick**：
Agent 执行的一次 shell 命令形成的 Tick，由 Tickhost 以管道运行（不使用终端），只在退出时更新；以 Bash 调用的描述（Description）为名。它不占用 Pane，与 Terminal Tick 是不同类型。
_Avoid_：后台任务、Task（只描述执行方式）

**Artifact Tick**：
以 Tickloop Artifacts 中的一个 Artifact 为实例的 Tick。它的更新只有 Artifact 反馈，经 Artifact 关联交给 Loop；人或 Agent 修改正文都不是 Tick Update。
_Avoid_：关联 Artifact（Artifact 关联专指反馈关系）

**Tick 包（Tick Package）**：
从 Tick 市场单独安装的一个程序 `tick-<包>`、一份清单 `tick-package.json` 与教 Agent 使用它的 Skill，按数据来源划分；程序打开后是扩展 Tick，可以用子命令提供多个 Tick。认证方式由包声明（`none`、`api_key`、`oauth2`），属于包而不属于 Tick 协议。安装与登录只由用户发起，不打开、不订阅 Tick，也不授予权限。
_Avoid_：Plugin、插件、builtin provider（Terminal、Shell、Artifact 与协议是 Tick 类型，不是数据来源）

**Tick 市场（Tick Marketplace）**：
列出可安装 Tick 包及其预编译下载的清单（`marketplace.json`）；目前只有官方市场 [tickloopai/ticks](https://github.com/tickloopai/ticks)。
_Avoid_：商店、Store

### Loop 与 Trigger

**Loop**：
每个 Chat 恰好一个、持有该 Chat 执行状态的任务单元：按输入队列一轮一轮执行，空闲时等待，Trigger 送达时继续。Loop 没有结束的终态，失败或被打断的只是当前一轮。
_Avoid_：多个 Loop、goal 列表、StartGoal

**打断（Interrupt）**：
停止当前一轮的执行，由 Esc 或运行中空输入的 Ctrl+C 发出；同时暂停 active 的 Goal 并结束自定节奏 `/loop`，不改变接收开关。

**停止（`/stop`）**：
让 Loop 全部停下：打断当前一轮、暂停 active 的 Goal、结束自定节奏 `/loop`、暂停接收开关，并停止本 Agent 的全部后代。
_Avoid_：用“停止”指离开界面

**输入队列（Input Queue）**：
Loop 待执行输入的唯一队列，按用户输入、Agent 消息（含子 Agent 完成通知）、Trigger 输入的优先级取用。
_Avoid_：收件箱、待办队列

**立即发送（Send Now）**：
Agent 工作时让一条消息马上开始新一轮的提交方式（`ctrl+enter`）：先打断当前一轮，再执行这条消息。普通提交只是排队。
_Avoid_：插队、Steer（Codex 用它指普通提交并入当前回合）

**接收开关（Receiving）**：
Loop 是否接收 Trigger 的唯一开关，只有接收中与已暂停两种状态；`/stop`、普通历史装载与 Trigger 输入长时间无法写入 Transcript 使其暂停，继续使其恢复。
_Avoid_：监听、武装（armed）

**继续（Continue）**：
用户恢复接收开关的明确操作：在界面选择继续，或在该 Chat 提交一条新的用户输入。斜杠命令、重新附着界面与 `/goal resume` 不算继续。

**Trigger（唤醒来源）**：
不经用户输入就让 Loop 继续执行、受接收开关控制的来源，共五种：Tick 订阅、用户在 Monitor 用 Send Update 一次性交付的 Tick 当前内容、Artifact 关联、Goal 续跑与定时任务。前三种送达的是数据，后两种是用户授权的指令；用户输入与 Agent 消息不是 Trigger。
_Avoid_：事件触发、定时器、钟点规则

**发送更新（Send Update）**：
用户在 Monitor 中把未订阅的扩展 Tick 或 Terminal Tick 的当前内容一次性交给 Agent，不建立订阅。

**定时任务（Scheduled Task）**：
照 Claude Code 的 `/loop` 与 cron 工具，让 Loop 按时间重复或在某一时刻执行一条用户授权的指令，是 Trigger 的时间来源；只属于根 Agent。计划可以是重复（`/loop` 间隔或 5 段 cron）、一次性或自定节奏。定时任务不是 Tick。
_Avoid_：钟点规则、定时 Tick

**自定节奏 `/loop`（Self-paced Loop）**：
不写间隔的 `/loop <指令>`：每轮由 Agent 用 ScheduleWakeup 决定下一次唤醒时间的定时任务，每个 Chat 至多一个。

**Goal（目标）**：
用户为 Chat 设定、要求 Agent 持续推进直到达成的完成条件；每个 Chat 同时至多一个，只属于根 Agent。状态为 active、paused、blocked、budget_limited 与 complete。
_Avoid_：用“任务”或“Loop 目标”指代 Goal

**Goal 续跑（Goal Continuation）**：
Goal 为 active 且 Loop 空闲时，宿主送达的一次让 Agent 继续推进的 Trigger 输入。

**完成核对（Completion Check）**：
Agent 报告 Goal 完成后，宿主发起的一次只看条件与已有证据、不执行工具的独立模型判断。

### Agent 与界面

**Agent**：
理解目标、进行判断并调用工具完成工作的 AI 执行者；在 Loop 中工作，既与用户对话，也在 Trigger 到达或 Goal 续跑时继续工作。

**Chat**：
用户与一个 Agent 的一段持久对话，用户通过它发起、观察和控制它唯一的 Loop；同一 Chat 可以同时在多个 Agent 界面中打开。Chat 不是 Tick，不能被订阅。

**Transcript（会话记录）**：
Chat 对话的持久记录，按发生顺序保存对话内容与 Loop 状态变化，重启后据此恢复 Chat。同一 Transcript 在多处继续时可以形成多条对话链，仍属于同一个 Chat。
_Avoid_：Session、会话存档

**Branch（分支）**：
从已有 Chat 的某个位置复制对话而得到的新 Chat，记录其来源，此后各自独立发展。
_Avoid_：Fork、分叉（分叉指同一 Transcript 中的多条对话链）

**Agent 界面（Agent View）**：
用户查看并与某个 Agent 交互的终端界面；同一 Agent 可以连接多个界面。离开界面（`/exit`、关闭 Agent Pane、空闲空输入时的 Ctrl+C）不影响 Agent 的执行。
_Avoid_：用“关闭 Agent”指离开界面

**Daemon（Agent 守护进程）**：
每个配置根一个的本机后台进程（默认配置根即每个系统用户一个），持有 Agent 目录与身份、View 连接和显示登记，启停并监督 Worker，按需拉起 Tickhost；它本身不调用模型和工具。
_Avoid_：owner、owner daemon、Agent 执行服务

**Worker（Agent 工作进程）**：
由 Daemon 为每个根 Agent 启动的进程，以库的方式调用 Harness，运行该 Agent 及其子 Agent 与分支的模型调用、工具、审批、Trigger 与 Transcript。其他宿主中运行 Harness 的进程不称 Worker。

**Monitor（观察列表）**：
Agent 界面中本 Chat 打开或订阅的 Tick Instance、Artifact、后台 Shell 与定时任务的统一列表，Goal 固定显示在底部。成员关系与订阅、所有权分别管理。
_Avoid_：Ticks 面板、任务列表

**未读更新（Unread Update）**：
Monitor 中一个实例的最近一次内容更新晚于本 Chat 最后一次处理它的状态；未读属于本 Chat 中的实例，不属于订阅。

**输入对话框（Input Dialog）**：
Agent 界面底部的唯一交互容器，打开时替换输入编辑区，承载权限请求、问题、选择器、确认与管理界面；同一时间只显示一个，其余排队。整页查看内容的界面（Agents 总览、Transcript、Diff）不属于输入对话框。
_Avoid_：弹窗、Modal、Popup

**提示建议（Prompt Suggestion）**：
Agent 完成一轮后为用户下一条提示提供的可选草稿，接受并提交后才成为用户输入；生成与显示不开始执行，也不构成授权。

**文件提及（File Mention）**：
输入框中以 `@` 选择文件或目录、把它加入本次输入的方式。

**Agent 工作上下文（Agent Work Context）**：
Agent 理解工作范围时使用的宿主环境信息，包括默认 Workspace 与已知 Tab、Pane；它与文件工作目录及 Agent 的查看位置分别表达。

**Agent 全局模式（Agents Overview）**：
在本机范围内查看各个 Agent 及其父子关系、并进入所选 Agent 界面的总览。

**当前 Agent（Current Agent）**：
某个 Agent 界面打开总览前正在显示的 Agent，即离开总览时回到的对象；与总览中的光标选择和宿主焦点分别表达。

**Output Style（输出风格）**：
Agent 回复用户时的表达方式（内置 Standard、Concise、Detailed 三种），只改变详略、结构与语气，不改变能力或授权。
_Avoid_：风格模式、Mode

### Subagent 与家族

**子 Agent（Subagent）**：
接受父 Agent 委派、在委派关系与授权范围内工作的 Agent。前台委派等待本次执行的报告，后台委派立即返回、完成后通知父 Agent。

**Agent 家族（Agent Family）**：
一个根 Agent 及它直接或间接委派的所有后代（最多三层）；成员关系由真实委派决定，子 Agent 继续委派不建立新家族。
_Avoid_：团队、Team（跨家族关系不存在）

**家族临时空间（Family Temp Space）**：
一个 Agent 家族共享的私有任务结果与临时文件目录（含 `scratchpad/`）；不包含其他家族的文件，也不改变成员的授权。

**Agent 代号（Agent Handle）**：
父模型为子 Agent 选择的短名称（如 `sleep-30-2`），在家族中唯一且不复用，是稳定 Agent 身份的别名。界面中的 `@` 是显示前缀，模型工具使用裸代号；代号不同于任务描述（Description）与 Chat 标题（Title）。

**Agent 颜色（Agent Color）**：
子 Agent 创建时分配、在家族中区分成员的显示颜色，此后不变；它只标识成员，不表达状态。

**Agent 消息（Agent Message）**：
家族内一个 Agent 发给另一个的任务方向、澄清或结果；来源与用户输入分别记录，不能充当用户授权或人工审批。

**定向用户输入（Directed Human Input）**：
用户从一个成员界面以 `@代号` 直接提交给同家族另一成员的真实人类输入，接收方仍受其委派授权上限约束。

**用户转述（User Relay）**：
Agent 引用经过核验的真实用户输入向成员转述的内容；转述仍是 Agent 消息，不是人工批准或新的用户授权。

**子 Agent 停止（StopSubagent）**：
停止目标成员及其后代的执行和等待，保留上下文、代号与界面；根可以停止任一后代，成员只能停止直接孩子。它不结束后台 Shell 或 Tick。

**子 Agent 完成通知（Subagent Completion Notification）**：
子 Agent 每次执行结束（完成、失败或被停止）向父 Agent 投递的结果、状态与用量记录，进入父 Agent 的输入队列；不充当用户输入或新增授权。

**协作任务（Shared Task）**：
同一根 Loop 内可分配、领取、追踪依赖和记录结果的工作项，单人与家族共用；由 Settings 的任务工具开关启用，默认关闭。领取者是当前负责人，任务不扩大其授权。
_Avoid_：TodoWrite、待办

### 权限与审批

**执行模式（Execution Mode）**：
控制 Agent 在当前 Chat 中如何获得工具执行许可，共四种：Auto（自动审批）、Ask（允许读取与工作目录编辑，其他受保护操作人工确认）、Plan（只读规划）与 Full Access（跳过普通审批，保留显式 Ask／Deny 与必须由用户参与的确认）。执行模式不扩大工具的执行边界或授权范围。
_Avoid_：权限模式、Permission Mode

**自动审批（Automatic Approval）**：
Auto 模式下，对未由确定性规则或允许的快路径决定的操作，按用户意图与授权范围进行判断的过程；它不授予新权限。

**审批方式（Approval Method）**：
自动审批所用的判断方式：LLM（当前连接的辅助模型，没有时用当前 Chat 模型）或 Judgment（当前由 Jev 提供）。审批方式与执行模式分别设置。
_Avoid_：LLM 模式、Jev 模式（均不是执行模式）

**权限规则（Permission Rule）**：
由程序按工具与操作内容匹配的确定性 Allow、Ask、Deny 规则，冲突时按 Deny → Ask → Allow 处理。来源有宿主规则、托管约束、项目约束与用户规则；显式 Ask 不能由自动审批代批。
_Avoid_：用“规则”泛指自动审批规则

**自动审批规则（Auto Approval Rule）**：
供审批方式结合用户意图理解的自然语言规则，分三类：Soft Deny（默认阻止、可由用户明确授权或 Soft Allow 解除）、Soft Allow（解除匹配的 Soft Deny）与 Hard Deny（不能被用户意图或 Soft Allow 解除）。它们不能覆盖确定性权限规则与宿主限制。

**审批依据（Approval Evidence）**：
一次自动审批交给审批方式的不可变快照：拟执行操作、生效规则与审批环境、目录与权限修订、当前 Loop 的真实用户输入，以及之前的工具调用、回复与 Agent 消息；超出容量时按规则裁剪并标明省略。
_Avoid_：审批上下文压缩（压缩指对话历史摘要）

**审批环境（Approval Environment）**：
用户为自动审批明确提供的可信目标、敏感资源与使用场景说明；它与拟执行操作和工具返回的数据区分来源。

**权限结果记录（Permission Decision Record）**：
一次工具操作最终权限决定的记录（直接放行、自动审批、人工确认或拒绝），附判定方式与原因；放行不等于执行成功，记录也不是长期规则。

**审批回退（Approval Fallback）**：
Jev 因技术失败或容量问题临时改用 LLM 审批、冷却后再尝试恢复 Jev 的过程；有效拒绝不回退。

**人工审批接管（Manual Approval Escalation）**：
自动审批受阻后把操作交给用户决定的处理方式；接管不产生长期 Allow 规则。

**自动审批拒绝计数（Auto Denial Tracking）**：
当前 Chat 中连续与累计的有效自动拒绝次数，达到阈值时触发人工审批接管；不按命令或工具分别计数，也不代表自动重试。

**授权范围（Authorization Scope）**：
用户允许 Agent 自主执行操作的明确边界，包括可访问的资源、可执行的动作和风险限制；自动审批不能越过必须人工确认或禁止的操作。

**工作目录信任（Working Directory Trust）**：
用户允许一个工作目录提供 Agent 项目配置与指令的决定，首次进入时必须由用户确认；Auto 与目录内容都不能代替这一确认。
_Avoid_：Workspace Trust、自动信任

### Skill

**Skill**：
教 Agent 如何组合已有工具完成一类工作的指令集；本身不提供状态访问、工具能力或额外权限。宿主分别控制模型按需加载与用户显式调用。

**Tickloop Skill**：
教 Agent 打开、操作与订阅 Tick 的系统级内置 Skill，名称为 `tickloop`，与工作目录无关；Tickhost 可用时自动激活，用户也可用 `/tickloop` 以它启动一个新 Agent。

### ID

**Tickloop ID**：
Tickloop 自己生成的对象身份（Chat、Loop、Run、输入、Tick Instance、订阅、Shell 任务、定时任务、协作任务等）：一律是 UUID v7，写给模型与用户时用 22 位 base62；Herdr、Provider、Artifact 服务给出的外部 ID 不算。规则见 `tick/ids`。
_Avoid_：短 ID、编号

### 宿主接口

本节只收录开源部分会碰到的概念；托管服务的内部组件不在本仓库描述。

**Tickloop Cloud**：
Tickloop 的托管服务，用户经 Web／App 使用 Chat 与 Work；Tickloop Artifacts 与官网也由它构建和部署，服务端不在本仓库。它与本地 tickloop 共用开源的 Harness、AgentAccess 协议与 Tickhost。
_Avoid_：云服务、server

**Harness**：
按 Chat 推进回合的开源库：运行 Loop、工具与权限审批、Subagent 与模型调用，只经 Seam（Transcript Store、Execution Environment、Model Endpoint、Credential Source）访问外部。
_Avoid_：Agent Runtime、engine

**无状态宿主（Stateless Host）**：
不在进程中常驻 Chat 状态的 Harness 宿主（如 Tickloop Cloud）：每次取得 Chat Lease 时用新的 Harness 推进一个回合，状态全部经 Transcript Store 读写；参考实现与契约测试在 `agent/test-support`。

**来源编号（Citation Number）**：
WebSearch 与 WebFetch 的来源在家族根 Chat 的来源表中获得的全局编号，之后不重排、不复用；宿主可选用它代替 Markdown 超链接引用。

**Chat Lease**：
无状态宿主中同一时刻只允许一个持有者推进某个 Chat 家族的租约，带递增 epoch；失去租约的持有者不能再写 Transcript 或发布 Event Stream。
_Avoid_：锁、session lock

**Event Stream**：
Harness 的宿主按 Chat 发布的实时事件（流式输出、工具进度与审批请求），每个事件带按 Chat 递增的 seq，供断线续传与回放；持久记录以 Transcript 为准。

**显示投影（Display Projection）**：
由 Transcript 派生、供界面展示的 Chat 内容，可随时从 Transcript 重建；只供模型使用的内容不进入显示投影。

**Execution Environment**：
Agent 的 Bash、文件工具与 Tick 实际执行的地方，以工具的形式接入 Harness，模型看到的工具不因执行位置而改变；本地是 Local Environment（本地 Folder 与 daemon 的 Tickhost）。没有 Execution Environment 的 Chat 没有 Folder，不接触本地文件与进程。
_Avoid_：手、hands（仅作口语）、执行器

**Folder**：
绑定到 Chat、供 Agent 读写文件和运行命令的目录，位于该 Chat 的 Execution Environment 中；本地 Chat 以启动时的工作目录为 Folder。
_Avoid_：Workspace（指 Herdr 工作台）、Project

**Work**：
绑定了 Folder 的 Chat，是 Chat 的一种状态而不是另一种对话实体；界面上与 Chat 并列为 Chat／Work。Tick（Artifact Tick 除外）只在 Work 中可用。
_Avoid_：Cowork、agent 模式、工作模式

**Tickloop Accounts**：
Tickloop 唯一的账户与登录服务，是标准 OIDC 提供方；Tickloop Cloud、Tickloop Artifacts 与本地 tickloop 都用它登录，终端用设备码登录。简称 Accounts。
_Avoid_：Account Service、IdP、认证服务、登录服务、SSO 服务

**Tickloop 账户（Tickloop Account）**：
一个人在 Tickloop 的唯一身份，按已验证邮箱唯一，可以挂多个上游登录身份。
_Avoid_：用户表、账号映射、关联账户

### Artifacts

**Tickloop Artifacts**：
为用户保存、呈现和管理 Artifact 的独立服务及其网页产品，登录经 Tickloop Accounts，终端命令为 `/artifacts`；简称 Artifacts。列表、内容库与多个内容的集合也称 Artifacts（如“所有 Artifacts”）。
_Avoid_：Artifact Service、Tickloop Docs、文档（泛指 Artifact 时）；用 Artifacts 指单个内容

**Artifact**：
用户拥有、由人或 Agent 创建并可独立访问的一个内容成果，类型只有 Doc、Page 与 Slides，创建后不变。单个内容及其属性、操作和关系都用单数（如“打开 Artifact”“Artifact 评论”）。
_Avoid_：用 Artifact 指集合；把普通网页、项目文档或编译输出称作 Artifact

**Doc**：
以写作、阅读和共同编辑为主的 Artifact（Markdown），适合报告、方案、规格与笔记。
_Avoid_：给类型名加中文译名或括号解释（Doc、Page、Slides、Slide 同此）

**Page**：
以视觉呈现和交互为主的 Artifact（HTML），适合看板、图表、解释页、小工具与界面原型。
_Avoid_：Design、Prototype（作为 Artifact 类型名时）

**Slides**：
以演示为主的 Artifact，由有序的 Slide 组成，可以逐张翻看、按章节总览和全屏放映。
_Avoid_：Deck、演示文稿、幻灯片、PPT（作为 Artifact 类型名时）

**Slide**：
Slides 中的一张，1920×1080 的固定画布，有稳定 ID，可以带演讲备注。
_Avoid_：Page、页面（指单张 Slide 时）

**Artifact 图片（Artifact Image）**：
上传到某个 Artifact、只属于它的图片文件，随该 Artifact 删除；目前只有 Slides 可以上传，以 `asset:<id>` 引用。
_Avoid_：附件、素材（指 Artifact 图片时）

**Artifact 实时正文（Artifact Live Content）**：
Artifact 当前保存的唯一正文；每次保存立即对所有能访问它的人可见。
_Avoid_：草稿、发布、工作副本

**Artifact 评论（Artifact Comment）**：
能访问 Artifact 的登录用户在其上发起的讨论线程，所有能访问的人看到同一份讨论；评论不等于已完成的正文修改。
_Avoid_：私有评论、标注模式

**Artifact 版本（Artifact Version）**：
实时正文在编辑告一段落时由服务自动保留的快照，用于查看与恢复，不控制别人看到的内容。

**Artifact 反馈（Artifact Feedback）**：
登录用户在评论中写给 `@tickloop` 的持久留言，没有 Agent 也能保存；它没有待处理、处理中、完成或失败的任务状态，会投递到该用户关联这个 Artifact 的每个 Agent。
_Avoid_：反馈请求、反馈任务（反馈没有任务状态；传输层的领取与确认只保证可靠投递）

**Artifact 关联（Artifact Association）**：
某个用户把自己 Agent 的 Chat 与一个 Artifact 建立的接收关系，是该 Chat 的 Loop 的一种 Trigger，逐条投递 Artifact 反馈；同一用户可以关联多个 Agent。它不授予权限、不改变所有权，也不是 Tick Subscription；Loop 暂停接收时反馈保留、不投递。
_Avoid_：评论绑定、订阅 Artifact

**Artifact 访问范围（Artifact Access Audience）**：
Artifact 允许谁访问：任何持有分享链接的人，或指定的人；与查看、编辑两级权限分别表达。

**Artifact 角色（Artifact Role）**：
账号在某个 Artifact 上的访问级别（所有者、编辑者或查看者），由 Artifacts 服务报告；Agent 一律按发起它的账号本人的角色操作 Artifact。
_Avoid_：可信来源、组织信任

**Artifact 链接授权（Artifact Link Access）**：
通过当前有效分享链接取得的访问资格，随链接停用或权限收紧而结束；不等于被邀请为成员。

### Herdr 与布局

**工作台（Workspace）**：
Herdr 中组织一组 Tab 的工作环境，用户可以拥有多个；Tickloop 不再定义额外的容器层。
_Avoid_：Space（旧 Tickloop 容器名称）

**Layout**：
Herdr 中 Workspace → Tab → Pane 的组织与排列，包括分割、尺寸、位置和焦点；与 Tick 的业务内容分别管理。

**布局宿主（Layout Host）**：
承载工作台布局与 Tick 视图的外部环境，当前为 Herdr；它只呈现视图，Tick 不在其中运行。

**Tab**：
Herdr 的 Workspace 中组织一组 Pane 的页面；显示名称与身份分别处理。

**Pane**：
Herdr 中承载终端会话的布局容器，可以拆分、调整尺寸和移动，也可以呈现 Tick 视图；Pane 身份与 Tick Instance 身份分别处理。
_Avoid_：Panel（旧 Tickloop 容器名称）

### 外观

**Theme（主题）**：
把界面语义角色映射到颜色与文本修饰的一组配置；不改变行为、布局、字形或文案。界面字形只有一套普通 Unicode 字符。
_Avoid_：Glyph Set、Icon Theme、Nerd Font

### 数据来源

**Source（数据源）**：
经 Tick 包向 Tickloop 提供数据的外部来源。改变外部状态的操作属于单独的写入能力，读取授权不扩大到写入。

### 版本与发布

**产品版本（Product Version）**：
一个独立发布单元的版本：Tickloop（含 `tickhost`）共享一个版本，每个 Tick 包各自独立；通信兼容性由各自的协议判断，不要求版本相等。

**发布提交（Release Commit）**：
一次发布中只记录产品版本文件变更的提交，并打 `vX.Y.Z` 标签。
