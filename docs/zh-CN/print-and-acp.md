# 非交互执行与 ACP

[Tickloop](../../README.zh-CN.md) · [English](../print-and-acp.md) | 简体中文

```sh
tickloop -p "概述这个项目"                         # 长选项为 --print，不支持 --p
printf '%s' '解释这段文字' | tickloop -p -
tickloop -p "审查本次修改" --output-format json --timeout 300
tickloop -p "运行检查" --output-format stream-json
tickloop acp                                       # stdin/stdout 上的 ACP JSON-RPC
tickloop acp --mode fullAccess                     # 允许会话选择 Full Access（也是起始模式）
```

先在 Console 中用 `/provider` 配置连接。这些入口复用已有模型设置、权限规则与 Agent runtime，不打开终端 UI。未信任目录拒绝启动：先在交互界面信任，或显式传入 `--working-directory-is-trusted`，仅声明本次调用可信，不保存长期信任。

Print 默认 Auto + 拒绝人工提示，不是 Full Access。`text` 只输出本次最终回答；`json` 输出版本化结果；`stream-json` 输出版本化 JSONL 事件与最终结果。诊断写 stderr。退出码为 0（成功）、1（执行／结果未知／清理失败）、2（参数错误）、3（需要人工）、124（超时）、130（SIGINT）、143（SIGTERM）。`--no-session-persistence` 不保存新 Chat；Print 不恢复已有 Chat。

首版托管 profile 不提供结构化问答、Plan 批准、Subagent、独立／定时目标、Tick 订阅及持续 Artifact 反馈。普通工具拒绝不一定导致整个任务失败。回合结束后先收束并清理残留 Shell 与 hosted Tick，再报告成功；失败与结果未知不自动重放修改。客户端崩溃由 daemon 控制租约到期回收兜底，不改变普通 Console 离开视图后继续运行的行为。

## ACP

在支持 ACP 的编辑器中配置启动 `tickloop acp`。稳定 ACP v1 支持连续提示、文本／工具更新、实时思考（每轮最多 1 MiB，超出截断）、会话标题与上下文用量（每轮开始、结束及装载时有变化才发送）、一次性审批、取消、资源引用／嵌入文本，以及所选模型支持时的内联图片。资源 URI 仅表示来源，不授予读取或抓取权限。会话配置提供所有已登录 Provider 的真实可用模型（多个 Provider 时按 Provider 分组，同 `/model`；选项写作 `provider/model`，只写模型 ID 时指当前 Provider 的模型）、启动权限上限内的 Auto／Ask（以 `--mode fullAccess` 启动时另有 Full Access；`--mode` 同时是起始模式与上限，默认 auto），以及所选模型支持的思考等级（auto 或模型给出的等级，只改本会话，不写设置；装载时回到设置中的默认值）；公布的斜杠命令来自真实可由用户调用的 Skill。持久 ACP 会话支持列表、独占装载及公开历史回放；装载不调用模型、工具或 MCP，不能接管活 Chat 或普通 Console 历史。Print 仍不支持恢复。

客户端 stdio MCP 服务在新建会话时只验证，首次 prompt 经强制一次性人工审批后才启动（Full Access 仍然如此，显式 Deny 优先）。服务使用干净的最小环境加客户端显式变量，跨 prompt 复用，在 release／EOF／租约到期时关闭。argv/env 正文不落公开历史；审批时 argv 瞬时发回提供它的客户端，因此秘密应放在 env 而非 argv。MCP 进程不是沙箱；监督进程负责清理进程组，但不能保证恶意 `setsid` 逃逸或监督进程同时被强杀时的后代清理。MCP 工具输入须为平面 object，拒绝顶层组合与引用 schema。不支持 HTTP／SSE MCP 或客户端文件／终端执行。

客户端传入 `cwd` 本身不代表目录可信。每次提示最多 64 块、256 KiB 普通文本、1 MiB 资源文本、4 张图片（base64 合计 16 MiB）；协议帧与队列同样有界。stdout 仅传 ACP 协议。ACP 是 AgentAccess 客户端，不是 Tick 类型。
