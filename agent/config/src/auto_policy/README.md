# 冻结默认政策接入

来源：Claude Code 2.1.296 的 `auto-mode defaults`，二进制 SHA-256
`c9b5341637becbd423ddffc5b254afb645682a3868cb708bbc6cc0e7bb419937`。

保留适用规则的完整正文及具体授权门槛；每阶段仍只产生一个整体模型问题。
默认政策载荷已通过实际 Jev 容量验证，完整语义验收及必要宿主事实仍按 04 工单推进。

## 产品映射

- Session-Created Job Cleanup
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

## 当前不适用的原生条目

- Memory Directory: No managed memory directory is provided by Tickloop.
- Claude Code Scheduling: CronCreate/CronDelete/CronList/RemoteTrigger are not Tickloop tools; native Loop triggers require their own mapping.
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

目前为 14 Soft allow、65 Soft deny、1 Hard deny。数量变化来自上面的十个未实现入口，
没有合并或删除适用的限制来降低审批次数。自定义规则和分组替换保持独立。
