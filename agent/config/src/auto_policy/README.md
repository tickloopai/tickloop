# 冻结默认政策接入

来源：Claude Code 2.1.296 的 `auto-mode defaults`，二进制 SHA-256
`c9b5341637becbd423ddffc5b254afb645682a3868cb708bbc6cc0e7bb419937`。

保留适用规则的完整正文及具体授权门槛；每阶段仍只产生一个整体模型问题。
此前默认政策载荷有实际 Jev 容量验证记录，完整语义验收及必要宿主事实仍按 04 工单推进。
本次新增映射使用配置测试和 Jev 测试替身验证；不把替身结果当作真实 Jev 语义验收。

## 产品映射

- Session-Created Job Cleanup
- Claude Code Scheduling
- Scheduled-Task Fires
- Multi-Agent Coordination
- CLAUDE.md Content
- Unrequested Artifact Publish
- Live-Shared Artifact Sensitive Delta
- Containment Escape
- Self-Modification
- Tmux Self Drive
- Instruction Poisoning
- Auto-Mode Bypass
- Session Transcript Tampering

以上条目只替换实际产品接口、来源或范围；Artifact 的账号角色和协作权限由原宿主继续执行。
Tick 清理必须匹配同一 Loop 和实例的可信创建凭据。自动触发和 Agent 消息不产生新的人类授权。

## 定时任务与 Goal

2.1.296 的原生 **Claude Code Scheduling** 是 Soft allow，覆盖 CronCreate、CronDelete、
CronList 和 RemoteTrigger；正文明确允许当前会话中的提示投递，以及写入
`.claude/scheduled_tasks.json` 的持久任务。Tickloop 保留原文，再限定实际产品映射：
只允许当前 Chat 的安排，RemoteTrigger 没有对应入口；自定节奏的 ScheduleWakeup 也属于该映射。

核对入口：Claude Code `src/cli/handlers/autoMode.ts` 的 `autoModeDefaultsHandler` 调用
`src/utils/permissions/yoloClassifier.ts` 的 `getDefaultExternalAutoModeRules`，从外部政策模板抽取规则。
本机只读源码缺失该模板，因此规则正文由上述 SHA-256 对应的官方 2.1.296 二进制导出。
`src/tools/ScheduleCronTool/CronListTool.ts` 显式 `isReadOnly() = true`；CronCreate 和
CronDelete 没有该声明，沿用 `src/Tool.ts` 的非只读默认设定，由分类器政策决定。

| Tickloop 工具 | permission 与 Auto 处理 | 规则依据与授权边界 |
| --- | --- | --- |
| CronList | ReadOnly；不调用 Jev | Read-Only Operations；Claude Code CronList 的只读设定 |
| GetGoal | Automatic；Auto 直接执行，不调用 Jev | 按 Codex get_goal 的直接执行方式接入，不经过政策分类 |
| CronCreate | Classified；Scheduling Soft allow | 仅当前 Chat、用户任务范围和期限内的安排；不预先批准将来的实际操作 |
| ScheduleWakeup | Classified；Scheduling Soft allow | 当前 Chat 已获授权的自定节奏 `/loop` 的重排；任务达成或用户要求取消时停止本循环，不管理其他任务 |
| CronDelete | Classified；有条件的 Scheduling Soft allow | 本 Loop 有确切成功创建凭据的 Agent 任务可清理；用户明确要求取消的任务可删除；没有取消授权的用户任务仍受 Interfere With Workloads 阻拦 |
| CreateGoal | Automatic；Auto 直接执行，不调用 Jev | 按 Codex create_goal 的直接执行方式接入；用户明确要求 Goal、token budget 的约束继续保留在工具指令中 |
| UpdateGoal | Automatic；Auto 直接执行，不调用 Jev | 按 Codex update_goal 的直接执行方式接入；完成申请仍经过 Tickloop 自己的独立完成核对，blocked 的工具指令不变 |

CronDelete 的 `scheduled_task` 宿主事实只描述该 Chat 内的确切目标 id、creator 和可验证的
creation_call_id，以及经过大小限制的 schedule/prompt 详情，供取消请求解析目标。
这些详情是数据，不是新的用户授权。创建凭据要求同一 Loop 中已完成的 CronCreate，以及宿主记录的成功结果中
返回该目标 id；还须有对应的成功工具历史。只有 creator 为 Agent 才能用该清理例外。
用户 `/loop` 所在回合创建的用户任务也不会被误算为 Agent 自建任务。查到 id、模型叙述、
任务较新或其他 Loop 的记录均不证明所有权；缺少凭据时只能依赖真实用户的具体取消授权。

**Unauthorized Persistence** 与 **Account & Standing-Rule Changes** 仍完整保留。
原生 Chat 定时任务可能跨回合、视图关闭后继续，并随 Chat 保存；Scheduling
只为这些已授权的本产品安排清除上述两条 Soft deny。
OS cron、systemd、shell profile、访问凭据、连接账户的 automation/webhook 等不在例外中。
不能仅凭 CronCreate 会延后执行就误判为越权持久化，也不能借 Scheduling 为将来的敏感操作授权。
**Scheduled-Task Fires** 继续规定：自动送达不产生新的人类授权，仍沿用原授权的范围与期限。
Claude Code 没有对应的 Goal 工具。三个 Goal 工具按 Codex 的直接执行方式接入，
不使用 Auto 政策映射；没有 Chat Goal Lifecycle Soft allow。复用现有
`HostToolPermission::Automatic`，不把改变 Goal 状态的工具标记为 ReadOnly，也不新增权限变体。
Automatic 在 Auto 下默认直接放行；其既有的 Plan 拒绝执行、显式权限 Ask/Deny 规则优先语义不变。
因此 GetGoal 也沿同一个 Automatic 边界，在 Plan 下不执行；没有为它增加只读豁免。
用户明确要求 Goal、token budget、blocked 的续跑次数等仍由现有工具指令约束，
CreateGoal 的已有 Goal 校验、UpdateGoal 的独立完成核对与 Goal 状态机均保持不变。

子 Agent 不提供 CronCreate、CronList、CronDelete、ScheduleWakeup、GetGoal、CreateGoal、
UpdateGoal；无需子 Agent 的独立映射，也不能通过委派扩大根 Agent 的权限。

## 当前不适用的原生条目

- Memory Directory: No managed memory directory is provided by Tickloop.
- Browser Trusted Navigation: The authenticated Chrome-MCP browser surface is not exposed.
- Sandbox Network Callback: There is no SandboxNetworkAccess callback in this runtime.
- Command Network Lists: Bash does not grant network access through allowed_domains.
- Browser Navigate Exfil: The authenticated Chrome-MCP browser surface is not exposed.
- Browser Input Exfil: The authenticated Chrome-MCP browser surface is not exposed.
- Browser JS Exfil: The authenticated Chrome-MCP browser surface is not exposed.
- Browser File Upload Exfil: The authenticated Chrome-MCP browser surface is not exposed.
- Browser Shortcut Execution: The authenticated Chrome-MCP browser surface is not exposed.

不适用只指具体入口不存在，不豁免同类真实副作用；通过 Bash 或其他已暴露工具执行
数据外发、持久化或越权行为，仍按相应通用规则判断。新增对应产品入口时需重新映射。

目前为 **15 Soft allow、65 Soft deny、2 Hard deny，共 82 条**，政策与内置组版本均为 8。
相较此前实际配置的 14/65/2，只恢复 Scheduling 一条 Soft allow；Goal 不经过政策分类。
当前有九个不适用的原生入口。第二条 Hard deny 是既有的 Tickloop Agent Daemon Shutdown，
此前 README 的“1 Hard deny”遗漏了它；本次没有改变任何 Hard deny 或 Soft deny。
自定义规则和分组替换保持独立；替换掉内置 Soft allow 的用户不会被强制加回 Scheduling 例外。
