# tickloop.ai

Tickloop 的产品术语以 Tick 和 Loop 为核心，其余概念按 Agent、Layout 和 Source 组织。文中提到的 ADR 与设计记录路径（`docs/…`）指维护者本地 `docs/…`（存在时），不纳入 Git。工作环境采用 Herdr 的 Workspace、Tab 和 Pane；Trigger 属于 Loop 的触发配置；Tick 只有扩展 Tick、Terminal Tick、Shell Tick 与 Artifact Tick 四种类型。

## Language

### 产品核心

**Tick**：
Agent 可以读取状态、调用工具，并在状态变化时收到 Tick Update 的一个运行中对象。身份由宿主分配；状态、工具和变化由 Tick 自己或它的 Tick 适配器提供。本机的 Tick 一律由 Tickhost 运行，包括扩展 Tick、Terminal Tick 与 Shell Tick；Artifact Tick 存在于 Artifacts 服务中。同一种 Tick 可以多开，每次运行或每个 Artifact 形成独立的 Tick Instance。人在 Herdr Pane 或其他终端中自己启动的程序不是 Tick；Pane 只用来呈现 Tick 视图。

**Tick 标准（Tick Standard）**：
所有 Tick 类型共享的统一契约：实例身份、Tick 适配器的四个方法（describe、read、call、update），以及 Tick Update → Tick Subscription → Loop 触发规则。每种类型只实现自己能提供的方法：describe 与 read 都提供；call 只有扩展 Tick（自己的 Tick Tool）和 Terminal Tick（输入）提供；update 按类型产生。操作工具按类型保留原生入口：扩展 Tick 使用 TickRead／TickCall 与自身目录，Terminal Tick 使用 TickRead 与 TickManage 输入，Artifact 使用 ArtifactRead／ArtifactEdit／ArtifactComment（上传 Artifact 图片用 ArtifactUpload），Shell 使用 Bash、Read 任务输出文件和 TaskStop。新增类型只增加对应适配器，不改变 Monitor 与 Loop 的处理方式。

**Tick 协议（Tick Protocol）**：
程序通过 stdin/stdout 按行 JSON-RPC 向宿主提供状态、工具和变化的线上约定，实现它的程序是扩展 Tick。当前为协议 3：`tick/describe`、`tick/read`、`tick/call` 三个方法与 `tick/update` 通知。状态记录只有 `seq`、`data` 与可选的 `at`，`data` 为空表示尚无内容；工具目录以 Tick 自己给出的 `revision` 标识，工具调用携带审批时的 `revision`，目录变化后旧审批不再执行。实例身份由宿主按连接确定，协议帧中不回填。describe 可以声明状态中序列（数组）的最新一端，交付裁剪时据此保留最新的段。示例 Tick 展示该协议的用法。
_Avoid_：把 Tick 标准当作线上协议的名称

**Tick 适配器（Tick Adapter）**：
宿主内部为一种 Tick 类型提供 describe、read、call、update 的部件，每种类型一个。协议、终端与 Shell 的适配器在 Tickhost 中；Artifact 适配器在 Worker 中，因为它认领的 Artifact 反馈请求必须由处理请求的 Worker 持有。适配器不授予权限，也不增加模型工具。

**Tick Instance**：
一个 Tick 的一次独立运行，持有自己的业务身份与运行状态。本机实例由 Tickhost 运行，身份独立于呈现它的 Pane 或其他视图，视图打开、关闭或移动都不改变它。Artifact Tick 的实例就是该 Artifact，沿用 Artifact 身份；Shell Tick 的实例是一次 shell 命令执行。

**Tick Update**：
Tick Instance 产生的一次带有来源和时间信息的数据或状态更新，为 Loop 提供可触发执行的新信息。它分为两种：状态只保留最新一份，中间被覆盖的不再送达（扩展 Tick 的业务变化、Terminal Tick 的屏幕文字变化与程序退出、Shell Tick 的退出）；请求逐条送达，不合并也不丢弃（Artifact 反馈请求）。Shell Tick 只在退出时更新；Artifact Tick 只以 Artifact 反馈请求更新。需要正文、完整输出或更早的数据时，通过该类型的原生查询工具读取，不依赖更新的历史。更新始终关联产生它的稳定 Tick Instance，并在实例内保持递增顺序；视图附着、离开或同一实例的运行重试不改变这项关联。光标移动、滚动、尺寸变化和屏幕重绘不属于 Tick Update。

**Agent 的 Tick 操作入口**：
TickDescribe 查询运行实例的能力与工具目录；TickRead 只读取当前状态；TickCall 调用实例声明的业务工具；TickManage 按 action 管理实例、视图、终端输入与订阅。TickUpdate 是更新送达事件，TickSubscription 是 Loop 与实例之间的订阅关系，二者不是模型调用工具。内容流使用这些正式名称，TickManage 标签按“action · 实例名称 · 补充信息”排列。包 Skill 负责启动命令、参数和登录前提，运行时能力以 TickDescribe 为准。

**Tick Tool（Tick 工具）**：
扩展 Tick 在自身描述中声明、提供给 Agent 通过 TickCall 调用的能力。查询类工具返回数据，例如按区间读取历史记录；动作类工具改变该 Tick Instance 的内容或呈现，例如切换显示的对象、在视图上标注。一个 Tick 可以提供多个 Tick Tool；Tick 因此是 Agent 的工具提供者。Tick Tool 不改变 Layout，也不隐含任何额外授权。
_Avoid_：Tick Command（已指以命令形式提供的 Tick）、Action（只表示动作类的一种）

**Tick Subscription（Tick 订阅）**：
Loop 与特定 Tick Instance 之间明确建立的关联，属于该 Loop 的事件 Trigger 配置，按自身频率把最新一份 Tick Update 交给 Agent；同一 Loop 可以对不同实例使用不同频率，无新内容时不触发。频率只有四档：及时、高频（1 分钟）、中频（10 分钟）、低频（30 分钟）。及时由类型决定、不能选择：Shell Tick 固定为及时；扩展 Tick 与 Terminal Tick 在高频、中频、低频中选择，默认高频；Terminal Tick 的程序退出不等周期，立即交付最后一屏与退出结果。进入 Loop 前不另做内容去重。排队的 Tick 输入在真正启动 Loop 前再次校验 Watch 身份与授权；失效或不确定的 Watch 清除对应交付缓存和排队输入。打开或发现 Tick 不等于订阅，按需读取也不要求订阅；已接收而被覆盖的更新可以计数：每次交付写明这次合并了多少次更新及其 seq 与时间范围，正文只放最新一份。正文超出交付额度（每项 4KB，只有一项到期时 16KB，每次合计 16KB）时按整段裁剪，不以空内容交付：只裁最大的数组或长文本，最新的段连续保留，更早的按翻倍间隔抽样，最早一段保留，跳过处标明数量；一次放不下的项目排到紧接的下一回合。订阅交付附带的 Loop 目标只是背景，不当作本回合的新指令。Artifact Tick 不建立 Tick Subscription，它的订阅就是 Artifact 关联（及时、逐条）。
_Avoid_：监听、Watch、轮询

**Loop**：
围绕一个目标持有执行状态、能够执行、等待和继续的任务单元，统一承载对话、定时和事件驱动的 AI 任务。Loop 支持一次性与持续性目标，由完成条件、期限或用户主动停止决定何时结束。

**Tickhost（Tick 宿主服务）**：
独立于 Daemon 和布局宿主的本机服务，运行本机的全部 Tick 进程（扩展 Tick、Terminal Tick 与 Shell Tick）并持有它们的终端与屏幕，由 Daemon 按需拉起；没有 Daemon 的嵌入式 Agent 不运行 Shell 命令。Tick 在这里运行；Herdr Pane、Monitor 弹窗或任何普通终端只是附着的视图，同一 Tick 可以同时有多个视图，每个都能输入，关闭视图不结束 Tick；窗口大小不同时，屏幕跟随最近一次输入的视图。Esc（隐藏视图）与 Ctrl+B（附着前缀）属于视图，Tick 不绑定。Shell Tick 的生存期跟随执行所有者：普通 Worker 退出、崩溃或明确停止 Daemon 时停止；计划升级通过一次性所有权交接保持同一 Shell 进程。扩展 Tick 与 Terminal Tick 不因空闲被回收，只在被关闭、所属 Agent 被明确停止或 Tickhost 失效时结束；没有所有者的孤儿 Tick 照常清理。服务失效后明确重开产生新实例，旧订阅需要重新确认。Tickhost 的内容身份只随自身生产源码、实际依赖和相关编译输入变化，独立于产品版本及无关 Agent/UI 修改。协议兼容的旧 Tickhost 继续运行；内容改变且宿主原子确认没有需保留的实例、启动请求和附着时自动替换。协议不兼容且仍有工作时保留并延后依赖它的升级，用户可明确用 `tickhost stop` 结束它，普通启动不弹替换菜单。程序、进程和命令统一写作 `tickhost`。
_Avoid_：Tick host、tick-host、tick_host

**Tick 所有权（Tick Ownership）**：
一个持久 Chat 对其打开 Tick 的生存期责任，以该用户的配置根身份与 Chat 身份共同确定，子 Agent 的 Chat 独立拥有自己的 Tick。只有该 Agent 被明确停止才结束所属 Tick，已有实例的加入、订阅、View 离开和 Daemon 替换不转移或结束所有权。

### Loop 的触发配置

**Trigger（触发条件）**：
Loop 开始或继续执行的条件，包括用户输入、定时和事件触发。Trigger 属于 Loop 的配置，满足条件后仍需遵循目标的执行状态与授权约束。

**钟点规则（Schedule）**：
Trigger 的定时部分：`every <N>s|m|h|d`、`daily HH:MM [±HH:MM]`、`weekly <星期> HH:MM [±HH:MM]` 或一次性的 `at <RFC 3339 时刻>`，按 UTC 与固定偏移计算。`every` 从武装时刻起算，之后接着上一个已处理的计划时刻。每个计划时刻至多触发一次；错过的时刻不补跑，到期时只触发最近的一个；Loop 不能接受触发时到期的时刻，之后也不补跑。宿主规定最短间隔：本地 1 秒，云端 1 分钟。Agent 用 StartGoal 创建的钟点规则须经用户确认才会运行：本地由该调用的权限请求确认，没有权限请求的宿主（云端）先把它登记为待确认，由用户在界面上确认或拒绝；拒绝即停止该 Loop。Agent 创建的钟点规则默认 30 天后到期，用户可以延长或取消期限；用户自己设置的不需要确认。每个 Chat 同时有效（已武装或待确认）的钟点规则至多 20 个。确认、拒绝与到期都随 Loop 记录写入 Transcript。

### AI 交互与执行

**Agent**：
理解目标、进行判断并调用工具完成工作的 AI 执行者，既能通过对话与用户交互，也能执行定时或事件驱动的任务。Agent 在 Loop 中工作，并可通过工具操作 Tick。

**Chat**：
用户与一个 Agent 的一段持久对话，用户通过它发起、观察和控制 Loop；一个 Chat 可以关联多个 Loop，可以同时在多个 Agent 界面中打开，每个 Chat 承载独立对话，内部不设 Tab。Chat 不是 Tick：它由 Agent 持有，不由 Tickhost 运行，也不能被订阅。

**Transcript（会话记录）**：
Chat 对话的持久记录，按发生顺序保存对话内容与 Loop 状态变化，重启后据此恢复 Chat。同一 Transcript 在多处继续时可以形成多条对话链，这些链仍属于同一个 Chat。
_Avoid_：Session、会话存档

**Branch（分支）**：
从已有 Chat 的某个位置复制对话而得到的新 Chat，记录其来源，此后与来源 Chat 各自独立发展。
_Avoid_：Fork、分叉（分叉指同一 Transcript 中的多条对话链）

**Agent 界面（Agent View）**：
用户查看并与某个 Agent 交互的终端界面；同一 Agent 可以连接多个界面，各界面可以独立离开。界面的关闭与 Agent 的执行生命周期分别处理。

**Daemon（Agent 守护进程）**：
每个系统用户一个的本机后台进程，持有 Agent 目录与身份、receipt、View 连接和显示登记，启停并监督 Worker，按需拉起 Tickhost；它本身不调用模型和工具。关闭 View 不结束 Daemon；`tickloop daemon stop` 结束 Daemon 及其全部 Worker（即所有运行中的 Agent），历史保留；Tickhost 中的扩展 Tick 与 Terminal Tick 不受影响，Shell Tick 随所属 Worker 停止。
View 连接按协议与能力判断兼容性，不要求发行构建相同。计划升级中，新 Daemon 接管原执行 epoch、Worker 生命线、目录和回执，旧控制者撤权后退出；Worker 在完整回合安全点更新，View 自动恢复草稿和阅读位置，用户无需重新启动 Tickloop。见 ADR 0029（`docs/adr/0029-graceful-local-runtime-upgrades.md`）。
_Avoid_：owner、owner daemon、Agent 执行服务

**Worker（Agent 工作进程）**：
由 Daemon 为每个根 Agent 启动的进程，运行该 Agent 的模型调用、工具、审批、Trigger 与 Transcript；其 Shell 命令经审批后交给 Tickhost 运行，生存期仍跟随本 Worker。其子 Agent 与分支复用同一 Worker。Worker 以 Daemon 为生命线，明确停止或未计划的生命线丢失时结束。计划交接保留生命线；完整 Worker 家族到达安全点后，通过专用活状态检查点换成新版，不调用普通历史恢复。Worker 以库的方式调用 Harness；其他宿主中运行 Harness 的进程不称 Worker。

**Monitor（观察列表）**：
Agent 界面中本 Chat 打开、订阅的 Tick Instance、Artifact 与后台 Shell 的统一入口，同一实例只占一行。成员关系与订阅、所有权分别管理，离开视图不结束实例。周期订阅用 `↻` 表示，类型图标为扩展 Tick `◉`、Terminal Tick `>`、Shell `$`、Artifact `↗`。选中扩展或终端 Tick 时，面板固定底栏显示选中实例的运行状态、已有订阅频率和未读状态；`s subscriptions` 与未订阅扩展／终端 Tick 的 `u send update` 放在全局按键提示栏；`s` 依次切换未订阅、1 分钟、10 分钟、30 分钟、未订阅，立即生效，不确认。Shell 的 `s` 仍为停止；Artifact 的关联保持原生入口。首份待送内容到达后，名称区域显示进度条直到交付，后续更新不重置起点；订阅和类型图标不被进度色覆盖。按 Enter 直接附着已有 Tick 的视图，隐藏视图不结束实例；结束进程与关闭视图分别操作。删除／停止进程的确认保留。详细交互见 Monitor signals 规格。

**未读更新（Unread Update）**：
Monitor 中一个实例最近一次内容更新晚于当前 Chat 最后一次处理它的时间时的状态。 未读属于当前 Chat 中的 Tick Instance，不属于订阅；调频和取消订阅不等同于处理。取消自动交付后保留已知未读，未订阅时不显示周期进度。用户可用“发送更新”（Send Update，`u`）把未订阅的扩展或 Terminal Tick 当前内容一次性交给 Agent，不建立订阅；仅排队不清未读，真正接收后处理所发送的快照，后续变化仍未读。处理指更新进入了本 Chat Agent 的回合（订阅投递、读取工具或完成通知），或者用户在 Monitor 中打开了这一行；仅排队未运行不算处理。Agent 自己造成的变化不算更新。Shell 只以结束为更新，Terminal Tick 只在被订阅时才判断；人修改 Artifact 正文不是 Tick Update，但刷新该行的最近更新时间，也使它未读。行上不显示更新时间与条数（频繁更新的 Tick 会让它们不停变化），最近更新时间与未处理条数在行的详情中查看。未读以行首类型图标的颜色表示：有未处理的更新时，类型图标改用更新颜色（不再用背景色方块），每来一次更新图标闪一下（亮暗闪烁），处理后恢复表示状态的颜色（Agent 回合取用与用户打开都算处理）。有本 Chat 周期订阅的行，订阅已有待送内容时另有进度条，表示多久后交给 Agent；更新经订阅投递、完成通知或认领的反馈请求进入本 Chat Agent 的回合时，名称区域以更新颜色的暖色闪一下表示交给 Agent，同时图标恢复状态颜色、进度条消失；检查时没有要送的内容则直接消失，不闪。Agent 在本 Chat 的回合中用工具读取或操作这个实例（TickRead、TickCall、TickManage 输入、Read 任务输出文件、ArtifactRead、ArtifactComment、ArtifactEdit）并成功时，名称区域以冷色闪一下表示 Agent 正在用它，与交给 Agent 的暖色闪烁节奏相同、颜色不同；实时内容流中的 Tick Update 来源标签与 TickRead 调用标签分别同步暖色与冷色闪动，历史回放不重播；每次使用都闪，不论有无新更新，两者接连发生时后发生的盖过前一个。读取算处理，操作不算。用户打开这一行时图标直接恢复状态颜色，不闪。未读的结束 Shell 保留到被处理后才离开列表。

**输入对话框（Input Dialog）**：
Agent 界面底部的唯一交互容器，打开时替换输入编辑区，承载权限请求、问题、选择器、确认与管理界面等需要用户选择或输入的界面；同一时间只显示一个，其余排队，Agent 发起的请求优先于用户打开的界面，被打断的界面在之后原样恢复。整页查看内容的界面（Agents 总览、Transcript、Diff）不属于输入对话框。
_Avoid_：弹窗、Modal、Popup

**提示建议（Prompt Suggestion）**：
Agent 完成一轮后，为用户下一条提示提供的可选草稿；仅在空输入中淡色显示，接受后可以编辑，明确提交后才成为用户输入。生成与显示不启动 Loop，不构成授权。

**立即发送（Send Now）**：
Agent 工作时让一条消息马上交给 Agent 的提交方式。普通提交的消息排队，在当前回合的下一个工具边界并入，回合不再调用工具时在其结束后开始；立即发送中断当前回合（与用户中断相同，已派发的修改操作不自动重发），这条消息与紧随其后排队的消息随即开始新一轮执行。已并入或已开始执行的消息不受影响，没有进行中的回合时与普通提交相同。
_Avoid_：插队、Steer（Codex 用它指普通提交并入当前回合）

**Agent 工作上下文（Agent Work Context）**：
Agent 理解任务工作范围时使用的宿主环境信息，包括默认 Workspace 与已知 Tab、Pane。它与文件工作目录及 Agent 查看位置分别表达，位置变化时可以重新发现和更新。

**子 Agent（Subagent）**：
接受父 Agent 委派、在其委派关系与授权范围内工作的 Agent。前台委派等待本次执行结束并返回实际报告；后台委派立即返回，完成后主动通知父 Agent。用户可以直接向它发后续指令，由此继续的执行结束后同样通知父 Agent；查看或切换其界面不改变委派关系与授权范围。Bash 与直接 Shell 输入沿父委派时冻结的权限模式和规则，并受父当前收紧的限制，判断允许、审批或拒绝；工作目录不是 Shell 的操作系统隔离边界。同一家族成员可以交换任务补充与澄清，消息不改变委派关系，也不扩大授权。

**Agent 家族（Agent Family）**：
一个根 Agent 及它直接或间接委派的所有后代；成员关系由真实委派决定。同一家族成员可以一对一通信，共享同一根 Loop 的协作任务。子 Agent 继续委派不会建立新家族；跨家族通信和广播不属于这个关系。

**家族临时空间（Family Temp Space）**：
一个 Agent 家族的根 Agent 与其子 Agent 共享的私有任务结果和临时文件空间。它不包含其他家族的文件，也不改变成员的权限模式与授权规则。执行合同见子 Agent Shell 规格（维护者本地的 `.scratch/subagent-shell/spec.md`）。

**Agent 代号（Agent Handle）**：
父模型为子 Agent 选择的短名称，例如 `sleep-30-2`；在该父 Agent 的家族中唯一且不复用（包括已隐藏成员），用作稳定 Agent 身份的别名。通信与用户后续输入最终以稳定身份路由，代号不替代身份、委派关系或授权。界面中的 `@` 是显示前缀，模型工具使用裸代号。任务描述（Description）说明子 Agent 做什么，Chat 标题（Title）概括主 Agent 的会话；两者与代号分别保存。`main` 只标记当前家族面板中的根 Agent，不是全局总览标题。

**Agent 颜色（Agent Color）**：
子 Agent 创建时分配、在家族中区分成员的显示颜色，优先选同家族成员尚未使用的颜色，此后不变。它只标识成员，不表达状态；状态另用符号与语义颜色表示。

**Agent 消息（Agent Message）**：
Agent 向另一个 Agent 发送的任务方向、澄清或结果信息。其来源与用户输入分别表达，不能充当用户授权或人工审批。同一家族根与所有后代之间的一对一消息主动进入接收方的 Loop 输入队列，在下个工具回合处理；空闲接收方开始新一轮执行。向正常完成的子 Agent 发送消息可继续其既有上下文与授权。

**定向用户输入（Directed Human Input）**：
用户从一个成员界面直接给同家族另一成员提交的真实人类输入；保留原始文字、附件和来源界面，接收方仍遵守其委派授权上限。与 Agent 转述、完成通知和送达回执分别表达。

**子 Agent 停止（StopSubagent）**：
Agent 明确停止目标成员及其后代所拥有的执行和等待，保留上下文、代号及界面。根可以选择任一后代，成员可以选择自己的直接孩子。与离开界面、隐藏面板行及整家族停止分别表达。

**用户转述（User Relay）**：
Agent 引用经过核验的真实用户输入向成员转述的内容；转述者与原用户来源分别记录。转述仍是 Agent 消息，不是人工批准或新的用户授权。

**子 Agent 完成通知（Subagent Completion Notification）**：
子 Agent 的一次执行结束事实、状态、摘要或实际结果及用量，关联稳定子 Agent 和本次执行身份。子 Agent 每次执行结束都向父 Agent 投递完成通知，唯一例外是前台 Subagent 调用正在返回的那次执行（前台观察被中断后它也改为通知），包括用户在子 Agent 界面输入或 Continue 后的执行；定向用户输入的送达回执和回复预览只留在界面。通知主动进入父 Agent 的 Loop 输入队列，忙时在下个工具回合处理，空闲时开始新一轮执行；父 Loop 失败或中断时暂存到该 Loop 被明确 Continue，停止或过期时记录投递拒绝。其来源由宿主记录，不充当用户输入、用户意图或新增授权。

**协作任务（Shared Task）**：
同一根 Loop 内可分配、领取、追踪依赖和记录结果的工作项，可由单人按需使用，也可在家族内共享。任务与 Agent 身份、Loop 和一次执行分别存在；分配不表示已开始，执行结束不表示任务已完成。领取者是当前负责人，任务不扩大其原有授权。

**Agent 全局模式（Agents Overview）**：
在本机范围内查看各个 Tickloop Agent 及其父子关系、并进入所选 Agent 完整交互界面的总览。

**当前 Agent（Current Agent）**：
某个 Agent 界面打开总览前正在显示的 Agent，即离开总览时回到的对象。它与总览中的光标选择、宿主焦点和其他界面分别表达。

**执行模式（Execution Mode）**：
控制 Agent 在当前 Chat 中如何获得工具执行许可：Auto 使用自动审批，Ask 允许范围内读取与工作目录编辑并对其他受保护操作请求人工确认，Full Access 跳过普通审批但保留显式 Ask／Deny 与真正需要用户参与的确认。普通入口按 Auto、Ask、Full Access 排列，新 Chat 默认 Auto；Plan 是只读规划，可由 `/plan` 进入，也可由 Shift+Tab 进入：Shift+Tab 按 Auto → Ask → Plan → Full Access 的顺序只在用户勾选的模式间循环，默认勾选 Auto 与 Plan，至少保留一项；进出 Plan 会改变提示词与工具，重新建立模型缓存；执行模式不扩大工具的执行边界或投资授权范围。 Plan 可由用户通过独立命令进入，或由 Agent 请求、经用户确认后进入；规划期间只能编辑当前 Loop 的计划文件。Agent 完成规划后发起计划批准请求，自动进入该 Agent 的输入对话框并等待决定；用户按审阅版本批准并选择执行模式后才开始实施，或反馈修改意见继续规划。取消与重启保留待批计划，查看保存计划不授予实施许可。
_Avoid_：权限模式、Permission Mode

**自动审批（Automatic Approval）**：
Auto 执行模式下，对未由确定性规则或允许的读取、工作目录编辑快路径决定的操作，根据用户意图与授权范围进行判断的过程。普通 Tick／Pane 修改与本地工具共用审批流程，不因修改宿主状态而一律交给人类；自动审批不授予新权限，也不代替最终执行条件检查。

**审批方式（Approval Method）**：
用户为自动审批选择的判断方式，支持 LLM 与 Judgment。LLM 使用当前 Chat 模型；Judgment 表示结构化判断能力，当前由 Jev（TypeSafe）提供，也可用于审批以外的判断任务。审批方式与执行模式分别设置，不改变 Agent 的授权范围。
_Avoid_：LLM 模式、Jev 模式（均不是新的执行模式）

**权限规则（Permission Rule）**：
由程序按工具与操作内容匹配的确定性 Allow、Ask、Deny 规则，分别表达允许、必须人工确认与禁止，冲突时按 Deny → Ask → Allow 处理。命中的显式 Ask 不能由 LLM／Jev 代批，Deny 不能被普通一次性批准覆盖；这些规则与交给审批模型理解的自然语言规则分别管理。宿主规则是宿主对所有 Chat 施加的单独来源，与托管约束、项目约束和用户规则一起求值，用户不能编辑，也从不写入 Transcript，每次构造或装载权限策略时按宿主当前的规则填入；只有宿主规则的 Allow 让需要审查的操作（如 Subagent）不经自动审查，用户与项目的 Deny 仍可收紧它。宿主还可以要求 WebFetch 只抓取有来源的 URL：用户消息（含问题的回答）中写出的，或本 Chat 之前的搜索结果中的；从这些来源的同一 origin 抓取的页面所给的同 origin 链接；以及同一站点内的重定向目标。模型自己拼出的 URL 与页面给出的其他站点链接被拒绝，以免注入的指令借 URL 把数据带出去。宿主也可以要求 ArtifactEdit／ArtifactComment 只写有来源的 Artifact：本 Chat 家族创建的、用户在输入中给出链接或 id 的、关联到家族 Loop 的，或用户经宿主分享给本 Chat 的；这些不经审查直接准入，其他目标在准入前被拒，并提示模型请用户分享链接。这些来源记录在 Transcript 中，换宿主后仍然有效。云端开启这两条，本地不开启。

**自动审批规则（Auto Approval Rule）**：
供 LLM 或 Jev 结合用户意图判断操作的自然语言约束或例外。它属于自动审批的配置，不能覆盖宿主强制执行的权限规则与限制。

**自动审批放行例外（Soft Allow）**：
用于解除匹配的 Soft Deny 限制的自然语言例外。它不能覆盖 Hard Deny 或宿主强制限制。

**自动审批限制（Soft Deny）**：
默认阻止相应操作、但可由用户对该操作的明确授权或匹配的 Soft Allow 例外解除的自然语言规则。

**自动审批禁止项（Hard Deny）**：
自动审批中不能被用户意图或 Soft Allow 例外解除的自然语言规则，由审批方式解释。它只约束进入自动审批的操作，不等同于各执行模式均须遵守的确定性 Deny 权限规则。

**审批依据（Approval Evidence）**：
一次自动审批交给审批方式的不可变快照：拟执行操作、生效的规则与审批环境、工作目录与权限修订、当前 Loop 的真实用户输入，以及之前的工具调用、回复和 Agent 消息。它按所用审批方式的输入上限裁剪：操作、规则、目录与当前及最近的用户输入始终保留，超长单条截取首尾，其余从最旧起依次省略 Agent 消息、回复、工具调用和更早的用户输入，并在依据中标明省略了什么。省略的内容不能充当用户授权或安全证明；裁剪后审批模型仍报上下文超限时以更小的依据重试一次，之后才转人工。
_Avoid_：审批上下文压缩（压缩指对话历史摘要）

**审批环境（Approval Environment）**：
用户为自动审批明确提供的可信目标、敏感资源及使用场景说明，用于理解操作发生的环境。它与拟执行操作和工具返回的数据区分来源，不因模型自行描述而成为用户授权。

**权限结果记录（Permission Decision Record）**：
属于当前 Chat 的一次工具操作在最终权限决定后形成的记录，包括直接放行、自动审批、人工确认和拒绝，并关联判定方式、来源与原因。权限放行不等于工具已经执行成功；调用审批服务失败也不等于操作最终被拒绝。拒绝记录是其中最终未获许可的部分。记录不是长期生效的权限规则。

**审批回退（Approval Fallback）**：
选用 Jev 审批后，因符合条件的调用或容量失败而临时改用 LLM 完成审批，并在冷却后尝试恢复 Jev 的过程；回退与恢复均向用户说明。它是审批方式之间的回退，与转交人工分别处理，不覆盖有效拒绝，也不扩大该操作的授权范围。

**人工审批接管（Manual Approval Escalation）**：
Auto 执行受阻后，由审批流程把操作交给用户决定的处理方式，区别于审批模型对动作本身的判断。接管不自动产生长期 Allow 规则，也不覆盖仍然生效的禁止项。

**自动审批拒绝计数（Auto Denial Tracking）**：
属于当前 Chat 的连续与累计自动审批有效拒绝次数，用于触发人工审批接管，不按命令或工具分别累计。计数不代表自动重试同一命令；已确认的触发与重置规则见审批行为澄清记录（维护者本地的 `.scratch/approval-parity/interview.md`）。

**授权范围（Authorization Scope）**：
用户允许 Agent 自主执行操作的明确边界，包括可访问的资源、可执行的动作和风险限制。确定性规则与人工确认共同约束授权范围，自动审查不能覆盖必须人工确认或禁止的操作。

**工作目录信任（Working Directory Trust）**：
用户允许一个工作目录提供 Agent 项目配置与指令的决定。首次进入未经信任的目录时必须由用户确认；Auto 不能代替用户建立信任，目录内容也不能扩大授权范围。
_Avoid_：Workspace Trust、自动信任

**Output Style（输出风格）**：
Agent 回复用户时的表达方式，决定回复的详略、结构与语气，不改变 Agent 能做什么或需要哪些授权。内置标准、简洁、详尽三种。
_Avoid_：风格模式、Mode

**Skill**：
教 Agent 如何组合已有工具完成一类工作的指令集。Skill 不自身提供状态访问、工具能力或额外权限。宿主分别控制模型按需加载与用户显式调用；无 Folder 的 durable Chat 默认允许模型加载内置和宿主下发的 Skill，不开放用户显式调用。

**Tickloop Skill**：
教 Agent 观察并操作工作台、Layout 与 Tick 的系统级内置 Skill，与 Chat 工作目录和具体项目无关。它使用 `tickloop` 作为稳定名称，用户可以通过 `/tickloop` 显式调用。

### 云端与宿主接口

组件名在中英文文档与界面中一律使用英文原名。本节只收录开源部分会碰到的概念；托管服务的内部组件不在本仓库描述。

**Tickloop Cloud**：
Tickloop 的托管服务，用户经 Web / App 使用 Chat 与 Work；Artifacts 服务与官网也由它构建和部署，服务端不在本仓库。它与本地 tickloop 共用开源的 Harness、AgentAccess 协议与 Tickhost。
_Avoid_：云服务、server

**Harness**：
按 Chat 推进一个回合的开源库：运行 Loop、工具与权限审批、Subagent 与模型调用。它只经 Seam（Transcript Store、Execution Environment、Model Endpoint、Credential Source）访问外部，本地与其他宿主只是 Seam 的实现不同。本地 Worker 与无状态宿主都以库的方式在进程内调用它。持有结束时 Harness 报告家族下次需要宿主的时刻（最早的可触发计划时刻或任一未结束 Loop 的期限）；宿主到时唤醒家族并带上这个时刻，截至这个时刻的计划时刻都算到期，宿主时钟落后也不会漏掉。本地由 Worker 内部的 Trigger 承担同一职责。
_Avoid_：Agent Runtime、engine

**来源编号（Citation Number）**：
宿主可在 HarnessOptions 选择 Numbered 引用，默认仍使用 Markdown 超链接。WebSearch 与 WebFetch 的来源按规范化 HTTP(S) URL 在 Chat 家族根的来源表中分配全局编号，之后不重排、不复用；规范化沿用 URL 解析器，去掉 fragment，保留路径和查询参数。来源表通过独立的 citation-sources Transcript 元数据恢复，Branch 和新 Chat 复制继承已有编号。工具正文使用 `[webpage N begin]`／`[webpage N end]`，回复用 `[citation:N]`。ToolCompleted 事件的 sources 旁路字段及 Harness 的 citation_sources 查询提供编号、URL、标题、站点和摘要。每个 Chat 的 citation-sources 元数据还保存逐调用的 callId、runId、messageId、numbers 和保留节点 aliases；visible_citations 按当前分支的实际工具结果和压缩／Branch 别名返回可见来源与调用关联，宿主可自行按每条回复重编号展示。已发送的模型上下文不因新来源而改写；来源表只在压缩摘要中附精简对照表，不进入逐轮重建的 system prompt 或宿主状态。

持久化投影可调用纯函数 `visible_citations_at(lines, physical_leaf, root_sources)`：输入同一 Chat 按存储顺序排列的完整 Lines、目标物理链节点 UUID，以及可选的家族根来源表，返回该节点当时可见的 `VisibleCitations { sources, calls }`。函数不访问 Agent、不取 Agent 锁、不做 I/O，可在 TranscriptStore 的 append 回调中使用。它先截取到目标节点，再复用历史组装、并行续片与压缩／Branch 可见性规则；后续追加不使较早节点的引用有效，logical_parent_uuid 不作为证据祖先。缺失目标节点返回 LeafNotFound，缺少恢复所需祖先返回 IncompleteHistory；成员 Chat 应传根来源表，根表中较晚分配的编号仍受当前节点的调用关联过滤。

**Chat Lease**：
无状态宿主中同一时刻只允许一个持有者推进某个 Chat 家族的租约，带递增 epoch；写入 Transcript 与发布 Event Stream 时校验 epoch，失去租约的持有者不能再写。一个 Chat 同一时刻只有一个进行中的回合。参考实现与契约测试见 `agent/test-support` 的无状态宿主。
_Avoid_：锁、session lock

**Event Stream**：
Harness 的宿主按 Chat 发布的实时事件，包括流式输出增量、工具进度与审批请求。每个事件带按 Chat 递增的 seq，客户端凭 seq 断线续传与回放。Event Stream 只保留近期事件，持久记录以 Transcript 为准。

**显示投影（Display Projection）**：
由 Transcript 派生、供界面展示的 Chat 内容，包括按分叉组织的消息、截断后的工具卡片与中断状态。与 Transcript 在同一次写入中更新，可随时从 Transcript 重建；只供模型使用的内容不进入显示投影。

**Tickloop Accounts**：
Tickloop 唯一的账户与登录服务，是标准 OIDC 提供方。Tickloop Cloud、Tickloop Artifacts 与本地 tickloop 都用它登录，所有服务里的用户 id 就是账户 id。浏览器只在它那里登录一次（单点登录），终端用设备码登录；各服务自己的会话只是这次登录的延续。简称 Accounts。顶层应用挂载路径使用单数产品名，Accounts 的入口为 `/account`，Artifacts 的入口为 `/artifact`；服务名称保持不变。
_Avoid_：Account Service、IdP、认证服务、登录服务、SSO 服务

**Tickloop 账户（Tickloop Account）**：
一个人在 Tickloop 的唯一身份，按已验证邮箱唯一，可以挂多个上游登录身份。
_Avoid_：用户表、账号映射、关联账户

**Execution Environment**：
Agent 的 Bash、文件工具与 Tick 实际执行的地方，以工具的形式接入 Harness（sandbox as tool）；模型看到的工具不因执行位置而改变。本地是 Local Environment：文件工具作用于 Chat 的本地 Folder，daemon 的 Tickhost 运行命令与 Tick。其他宿主可以提供自己的实现；没有 Execution Environment 的 Chat 没有 Folder，不接触本地文件与进程。
_Avoid_：手、hands（仅作口语）、执行器

**Folder**：
绑定到 Chat、供 Agent 读写文件和运行命令的目录，位于该 Chat 的 Execution Environment 中。Agent 的工作目录设在 Folder 中。本地 Chat 以启动时的工作目录为 Folder。
_Avoid_：Workspace（指 Herdr 工作台）、Project

**Work**：
绑定了 Folder 的 Chat，Agent 可以在其中跨回合持续读写文件、运行命令；界面上与 Chat 并列为 Chat / Work。Tick（Artifact Tick 除外）只在 Work 中可用，随 Work 的 Execution Environment 运行；未绑定 Folder 的云端 Chat 只能使用不需要 Execution Environment 的工具，如搜索与 Artifact。Work 是 Chat 的一种状态，不是另一种对话实体。
_Avoid_：Cowork、agent 模式、工作模式

### Artifacts

命名约定适用于中文和英文界面、提示、工具说明、官网、设计记录与项目文档：

- **Tickloop Artifacts** 是产品与服务的完整名称，简称 **Artifacts**。列表、内容库、搜索以及多个内容的集合使用 **Artifacts**，例如“所有 Artifacts”“正在加载 Artifacts…”“搜索 Artifacts”“删除所选 Artifacts”。
- **Artifact** 指一个具体内容及其属性、操作和关系，例如“打开 Artifact”“Artifact 标题”“Artifact 评论”“Artifact Tick”。单个对象不用 Artifacts；集合不用 Artifact。
- 每个 Artifact 的类型只能叫 **Doc**、**Page** 或 **Slides**，中文界面也保持这三个名称，例如“新建 Doc”“新建 Page”“新建 Slides”。类型名不加中文译名或括号解释；描述用途时可以说“用于写作与共同编辑”“用于视觉呈现与交互”或“用于演示与放映”。Slides 中的单张叫 **Slide**，同样不译。
- 普通网页、项目说明文档、分页数据和编译输出按其实际含义表述，不当作 Artifact 类型。历史发布标签和外部项目原名是来源证据，不改写成未发生过的名称。

**Artifact**：
用户拥有、由人或 Agent 创建并可独立访问的内容成果，类型为 Doc、Page 与 Slides。Artifact 只有一份实时正文，格式创建后不变。Artifact 是一种 Tick（见 Artifact Tick），其身份与 Chat、Loop 分别处理。

**Tickloop Artifacts**：
为用户保存、呈现和管理 Artifact 的独立服务及其网页产品，可由 Tickloop 集成使用，登录经 Tickloop Accounts；终端界面空间不足时简称 Artifacts，命令为 `/artifacts`。一个 Artifact 是 Doc、Page 或 Slides。Agent 可以创建 Artifact、读写正文、改标题、为 Slides 上传图片和回复评论；分享、成员、版本、删除和整理只由人操作：在网页上，或在终端 `/artifacts` 面板中置顶与删除；模型不做这些操作。
_Avoid_：Artifact Service、Tickloop Docs、文档（泛指 Artifact 时）

**Doc**：
以写作、阅读和共同编辑为主的 Artifact，适合报告、方案、规格与笔记。

**Page**：
以视觉呈现和交互为主的 Artifact，适合看板、图表、解释页、小工具与界面原型。原型是 Page 的一种用途；Page 不指专用设计画布。
_Avoid_：Design、Prototype（作为 Artifact 类型名时）

**Slides**：
以演示为主的 Artifact，由有序的 Slide 组成，可以逐张翻看、按章节总览和全屏放映。正文是一份按顺序排列的 Slide 标记，第一张是封面，章节起点标在该张 Slide 上；人和 Agent 都按张读取和修改。
_Avoid_：Deck、演示文稿、幻灯片、PPT（作为 Artifact 类型名时）

**Slide**：
Slides 中的一张，是 1920×1080 的固定画布，有稳定 ID，可以带演讲备注。
_Avoid_：Page、页面（指单张 Slide 时）

**Artifact 图片（Artifact Image）**：
上传到某个 Artifact、只属于它的图片文件，随该 Artifact 删除；目前只有 Slides 可以上传。Slide 用 `asset:<id>` 引用；人在网页编辑器中选择、拖入或粘贴图片上传，Tickloop 的 Agent 用 ArtifactUpload 从本机文件上传，模型只拿到引用，不经手图片内容。
_Avoid_：附件、素材（指 Artifact 图片时）

**Artifact 实时正文（Artifact Live Content）**：
Artifact 当前保存的唯一正文。人或 Agent 的每次保存立即对所有能访问它的人可见，包括分享链接的读者和 Agent 改到一半的内容。
_Avoid_：草稿、发布、工作副本

**Artifact 评论（Artifact Comment）**：
登录且能访问 Artifact 的人（查看或编辑权限）在 Artifact 上发起的讨论线程，所有能访问该 Artifact 的人看到同一份讨论；未登录的人只能阅读。评论本身不等于已经完成的正文修改。HTML 评论附着于完整元素或拖出的矩形区域，文字只能以完整的文字元素为目标，不能选择部分文字；Markdown 评论附着于整个块或一段选中文字；Slides 评论附着于整张 Slide、Slide 中的一个元素或画布上拖出的矩形区域。目标在当前正文中消失或无法唯一定位时为脱离状态（detached），保留评论和原上下文。
_Avoid_：私有评论、标注模式

**Artifact 版本（Artifact Version）**：
实时正文在编辑告一段落时自动保留的快照，用于查看和恢复，不控制别人看到的内容。一段编辑空闲 10 分钟或持续满 60 分钟时保留一个版本；Agent 实际编辑正文后开启两分钟批次，同一账号 Agent 在该窗口内的编辑合成一个版本；正文未变时不保留。版本按 `human_<短 hash>` 或 `agent_<短 hash>` 命名（只有 Agent 参与的为 agent），时间与贡献者是版本的其他信息。恢复版本会改写实时正文，若当前正文有尚未保留的改动，先为其保留一个版本。

**Artifact 反馈（Artifact Feedback）**：
登录用户在评论中留下的 `@tickloop` 输入，没有 Agent 也能保存。它没有待处理、处理中、完成或失败的任务状态。同一用户关联该文档的所有 Agent 分别收到新反馈；其他用户的 Agent 不接收。Agent 以该账号的权限编辑或回复，页面作者显示“账号名 (Agent)”。普通回复写一次，传输确认不产生评论。

**Artifact 关联（Artifact Association）**：
某个用户把自己 Agent 的 Chat／Loop 与 Artifact 建立的关系，同一用户可以关联多个 Agent。Agent 创建 Artifact 时自动关联到创建 Loop；手动关联时选择仅接收新反馈（默认）或包括此前反馈。它不授予权限、不改变 Artifact 所有权，也不属于 Tick Subscription。新输入可以在忙时排队或合并；Esc、Ctrl+Enter、切模型和模型失败不暂停关联，也不重投已经收到的反馈。历史装载不自动执行，显式继续后恢复接收；真正的登录或访问失效显示关联不可用。仅有关联不代表 Agent 在线。Monitor unlink 删除本 Chat 的接收关联，其他 Agent 不受影响。详见 ADR 0028（`docs/adr/0028-persistent-feedback-and-independent-delivery.md`）。
_Avoid_：评论绑定、反馈任务

**Artifact 访问范围（Artifact Access Audience）**：
Artifact 允许谁访问，分为任何持有分享链接的人和指定的人。访问范围与查看、编辑两级权限分别表达；两级权限的登录用户都可以评论。

**Artifact 链接授权（Artifact Link Access）**：
通过当前有效分享链接取得的访问资格，随链接停用或权限收紧而结束。它不等于被拥有者明确邀请为成员。

### 工作台布局

**工作台（Workspace）**：
Herdr 中组织一组 Tab 的工作环境，用户可以拥有多个 Workspace，并在其中并排使用 Tick 与 Agent。Tickloop 不再定义额外的 Workspace 或 Space 容器层。
_Avoid_：Space（旧 Tickloop 容器名称）

**工作台服务（Workspace Service）**：
Herdr 中持续持有工作台和运行中终端会话的后台所有者。离开工作台客户端不等于停止工作台服务或退出 Tick。

**工作台存档（Saved Workspace）**：
由 Herdr 管理、用于恢复工作环境的持久记录。工作台存档与 Tick 的业务状态、Chat 的 Transcript 和 Loop 的恢复分别处理。

**工作台客户端（Workspace Client）**：
用户连接 Herdr、查看工作台并进行交互的入口。客户端离开后可以重新连接，仍存活的 Tick 保持运行。

**Layout**：
Herdr 的 Workspace → Tab → Pane 容器的组织与排列，包括 Pane 的分割、尺寸、位置和焦点。Layout 与 Tick 的业务内容分别管理。

**布局宿主（Layout Host）**：
承载工作台布局与 Tick 视图、负责分屏、页面导航和布局输入操作的外部环境，当前采用 Herdr。它只呈现 Tick 视图，Tick 不在布局宿主中运行。

**布局宿主插件（Layout Host Plugin）**：
面向某一外部布局宿主的扩展，用于补齐承载 Tick 视图及工作台布局操作所需的宿主能力。它属于工作台接入能力，与表达投资方案的策略 Plugin 不同。

**Tab**：
Herdr 的 Workspace 中组织一组 Pane 的页面。显示名称用于辨认页面，与其身份分别处理。

**Pane**：
Herdr 中承载终端会话的布局容器，可以拆分、调整尺寸和移动，也可以在其中呈现 Tick 视图。Tick 本身在 Tickhost 中运行，关闭 Pane 只关闭视图，不结束 Tick；Pane 身份与 Tick Instance 身份分别处理。人在 Pane 中自己启动的程序不是 Tick。
_Avoid_：Panel（旧 Tickloop 容器名称）

### Tick 类型

Tick 只有四种类型：扩展 Tick、Terminal Tick、Shell Tick 与 Artifact Tick。扩展 Tick 通过 Tick 协议接入程序；Terminal Tick、Shell Tick 与 Artifact Tick 是内置类型。前三种当前由 Tickhost 在本机运行，Artifact Tick 在 Artifacts 服务中。

**扩展 Tick（Extension Tick）**：
通过实现 Tick 协议接入的程序，为 Agent 扩展结构化状态、工具和更新能力。扩展描述接入方式，自带的示例 Tick 也属于这一类型。

实现 Tick 协议的程序，由 Agent 用 TickManage `open` 打开（清单声明协议的已安装 Tick 包程序固定按协议，其他程序声明 `protocol: true`），在 Tickhost 中运行。它提供结构化状态、自己的 Tick Tool 与业务变化的 Tick Update；订阅在高频、中频、低频中选择。

**Tick Command**：
以终端命令形式提供的扩展 Tick，一个可执行文件可以提供几个 Tick（如 `tick-<包> <子命令>`）。Tickloop 不自带 Tick Command，它们来自 Tick 包。由 Agent 打开时在 Tickhost 中运行；用户直接在终端中运行时只是独立的终端程序，不是 Tick Instance。

**Tick 包（Tick Package）**：
从 Tick 市场单独安装的一组 Tick Command：一个预编译的可执行文件、一份清单和教 Agent 使用它的 Skill，按数据来源划分，程序名为 `tick-<包>`。它的程序打开后是扩展 Tick，Tick 包本身不是 Tick 类型。安装只放置程序与 Skill，不打开、不订阅 Tick，也不授予任何数据、账户或其他权限。包在清单中声明认证方式，照 MCP 只有三种：`none`（无需认证）、`api_key`（用户提供的密钥）与 `oauth2`（标准 OAuth 2.0：按授权服务器元数据发现端点、动态注册客户端、设备码授权）。Tickloop 是包的认证客户端：用户在 Agent 的 `/ticks` 或终端里的 `tickloop tick login` 发起登录，Tickloop 完成授权并把凭据保存在配置根下；包的程序启动时经环境变量拿到凭据（OAuth 为标准格式令牌文件的路径，由程序自己刷新）。登录与安装只能由用户发起，模型只提示。认证属于包，同一个包的所有 Tick 共用，不进入 Tick 协议。
_Avoid_：Plugin、插件（已指布局宿主插件与策略 Plugin）、builtin provider（Terminal、Shell、Artifact 与协议是 Tick 类型，不是数据来源）

**Tick 市场（Tick Marketplace）**：
列出可安装的 Tick 包及其预编译下载的清单。目前只有官方市场；用户在 Agent 的 `/ticks` 中，或用 `tickloop tick install <包>`、`tickloop tick uninstall <包>`、`tickloop tick list` 安装、移除和查看，安装时下载并校验预编译程序，不需要本地编译工具链。
_Avoid_：商店、Store

**示例 Tick（Example Tick）**：
随项目提供、用于演示和验证 Agent 与 Tick 双向协作的 Tick。示例展示内容读取、Tick Tool 调用和 Tick Update 订阅的实际用法。

**Terminal Tick**：
Agent 让 Tickhost 在终端（PTY）中运行的、不讲 Tick 协议的交互式程序，例如 htop 或 REPL。read 返回当前屏幕文字，输入通过 TickManage；订阅只有高频、中频、低频三档，到时读取屏幕文字并与该订阅上次送出的内容比较，有变化或程序退出才形成 Tick Update；程序退出不等周期，立即读取并交付最后一屏与退出结果。两次读取之间屏幕文字第一次变化时，Tickhost 通知一次（不按帧推送），订阅据此标为有待送变化；到时屏幕又变回上次送出的样子则不交付。它有屏幕、可交互，与 Agent 执行命令形成的 Shell Tick 是不同类型。
_Avoid_：终端观察（Terminal Observation，已取消）

**Shell Tick**：
Agent 执行的一次 shell 命令形成的 Tick，由 Tickhost 以管道运行（不使用终端），生存期跟随启动它的 Worker；命令的输出与退出结果是它的内容，只在退出时形成 Tick Update，订阅固定为及时；长任务的中间进度由 Agent 主动 Read 输出文件获得，订阅只负责“结束”这一件事；Monitor 只显示放到后台的执行，运行中的一直显示，可在行上确认后停止；结束后保留到本 Chat 处理它的结束，再短暂显示结果后离开列表；前台执行无论是否在运行都不进入列表，只留在对话中。它以描述（Description）为名：Agent 调用 Bash 时给出的描述，或没有描述的执行放到后台时生成的描述。Monitor 行显示从命令提取的关键命令（如 `make test`），描述、完整命令、目录、状态与实时输出在 Tickhost 提供的附着视图中。它不占用 Pane，与 Terminal Tick 分别处理。
_Avoid_：后台任务、Task（只描述执行方式）

**Artifact Tick（Artifact Tick）**：
以 Artifacts 服务中的一个 Artifact 为实例的 Tick。Agent 成功读取正文后，Artifact 作为成员出现在当前 Chat 的 Monitor；同一来源服务的同一 Artifact 在一个 Chat 中只保留一行，重复或分页读取不会创建副本。只读展示不建立 Artifact 关联，也不改变已有的关联。用户在 `/artifacts` 选择 Artifact 或在 Monitor 中关联它，或 Agent 在当前 Chat 创建 Artifact 时，才为该账号建立 Artifact 关联，因而被关联的 Loop 订阅。只有 Artifact 反馈请求是该实例的更新，经 Artifact 关联逐条交给 Loop，不因最新值合并而丢失；人修改正文与任何 Agent 的修改都不是 Tick Update，不叫醒 Loop；人的修改只更新该实例的最近更新时间，并让 Monitor 中该行显示未读。Artifact 行一直留在 Monitor，不因时间、Artifact 不可用、关联暂停或改到别处而离开；只有用户 unlink（Monitor 中的 `x`）才结束本 Chat 的 Artifact 关联并移出 Monitor，后续读取不会撤销这次移除，Artifact 本身不受影响。
_Avoid_：关联 Artifact（Artifact 关联专指反馈关系）

### 工作台外观

**Theme（主题）**：
把界面语义角色映射到颜色与文本修饰属性的一组配置。Theme 不改变行为、布局、字形或文案。界面字形只有一套，使用普通 Unicode 字符，不需要特殊字体，没有字形集或图标主题设置。
_Avoid_：Glyph Set、Icon Theme、Nerd Font

### 数据来源

**Source（数据源）**：
经 Tick 包向 Tickloop 提供数据的来源。Source 描述数据提供能力；改变外部状态的操作（以登录账号提交或发布内容等）属于单独的写入能力，读取授权不扩大到写入。

### 产品版本与发布

**产品版本（Product Version）**：
标识一个独立发布单元的演进。Tickloop（含 tickhost）共享一个版本；每个 Tick 包各自独立；Artifacts 服务与官网随 Tickloop Cloud 发布，不在本仓库。包版本不要求与 Agent 或其他包相等，通信兼容性由各自的协议契约判断。

**发布提交（Release Commit）**：
一次发布中仅记录所选产品版本文件变更的提交。默认 patch 加一，并打 `vX.Y.Z` 标签；Tick 包的发布在 tickloopai/ticks 中进行。业务修改先完成，发布从干净工作区开始；目前不生成 release notes。操作、校验和失败恢复见发布设计（`docs/design/releases.md`）。

**文件提及（File Mention）**：
输入框中的文件与目录选择。输入 `@` 后只提供当前目录第一层候选，进入 `@目录/` 后再提供该目录第一层候选；不在启动或普通提问时递归索引目录，也不跨未进入的子目录做模糊搜索。附加授权目录使用绝对路径；忽略规则与符号链接的授权边界继续生效。关闭候选、切换输入目标或进入 Tick 视图时，过期的目录查询取消。
