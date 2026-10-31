# Hook

[Tickloop](../../README.zh-CN.md) · [English](../hooks.md) | 简体中文

Hook 在 Agent 工作的固定时刻运行 Shell 命令：工具调用前后、你发送提示时、对话开始或结束时、一轮即将结束时，以及压缩对话之前。Tickloop 读取 Claude Code 格式的 `command` Hook；支持的事件和字段可直接沿用，兼容范围与差异见下文。Hook 有三个来源：

- **你的设置**：`settings.json` 顶层的 `hooks` 对所有目录生效，`projects["/absolute/project/path"]` 里的只对该工作目录生效，两者同时生效。
- **已启用的插件**：插件的 `hooks/hooks.json`，或其清单的 `hooks` 字段（路径或内联对象）。
- **仓库**：工作目录的 `.claude/settings.json` 与 `.claude/settings.local.json`。只读其中的 `hooks`，这两个文件里的其他设置一律不读。

```json
{
  "hooks": {
    "PreToolUse": [
      {"matcher": "Bash", "hooks": [{"type": "command", "command": "~/bin/check-command.sh", "timeout": 30}]}
    ],
    "PostToolUse": [
      {"matcher": "Edit|Write", "hooks": [{"type": "command", "command": "\"$CLAUDE_PROJECT_DIR\"/scripts/format.sh"}]}
    ]
  }
}
```

## 信任与 `/hooks`

你自己的 Hook 直接运行。插件和仓库的 Hook 要你信任后才运行，信任绑定其内容：

- 插件 Hook 的信任覆盖声明和安装时记录的随包文件。更新、重装或编辑改变这些内容时要重新确认；内容完全相同的重装不会单独使信任失效。安装后新生成的文件不在清单内，但随包链接仍覆盖其目标全部内容，链接目录中的新增文件也会改变指纹；没有可读清单的旧安装则覆盖整个插件目录。
- 仓库 Hook 的信任覆盖它的声明与该目录的 `.claude/hooks/` 目录：`git pull` 改动了其中任一处都要重新确认。命令调用其他位置的文件不在覆盖范围内，改动它们不会要求重新确认。

文件的完整相对路径也属于信任内容；移动脚本会要求重新确认。升级到 v3 指纹后，旧指纹需重新确认一次；读取或计算指纹失败时跳过 Hook 并给出警告。

确认之前这些 Hook 不运行，对话中会说明，例如 `PreToolUse:Bash hooks not run: 1 hook not confirmed yet · review in /hooks`。只有你能信任 Hook，Agent 没有办法代你确认。运行中从不停下来询问：任何对话（包括 Subagent 和无人观看的对话）都直接跳过未确认的 Hook。

`/hooks` 按 **Your settings**、**This folder** 与 **Plugin NAME@MARKETPLACE** 分组列出当前目录的 Hook。每行显示事件、matcher、`if` 条件、命令和状态：`trusted`、`pending`（从未信任）、`modified`（信任后有改动）或 `disabled`。用 `ctrl+n`/`ctrl+p` 或方向键选择，`enter` 查看详情（完整命令、所在文件、超时、`if` 条件、后台方式，以及信任覆盖的范围；放不下时用方向键或 `ctrl+n`/`ctrl+p` 滚动），`space` 或 `t` 信任，三秒内按两次 `x` 停用或恢复已停用的 Hook，`r` 刷新，`esc` 关闭。你自己的 Hook 也可以停用，停用后在所有目录都不运行。面板不编辑 Hook，请到它所在的文件里修改。信任记录保存在你的 `settings.json` 的 `hookTrust` 中（仓库的记在该目录的 `projects` 条目里），卸载插件时一并删除。

Console 打开目录时和 `/hooks` 中都会显示不支持的 Hook 配置警告，包括 `.claude/settings.local.json`。用列表的导航键也能到达 Hook 行下面的警告。

新对话开始时，如果当前目录有待确认或已修改的 Hook，会显示 **Hooks need review** 菜单：逐个审查（打开 `/hooks`）、全部信任并继续，或不信任直接继续（这些 Hook 不运行）。每次打开 Tickloop，每个目录只提示一次。

## Hook 能做什么

Tickloop 为以下十个事件运行 `command` 类型的 Hook，通过 stdin 接收 JSON，并读取退出码与支持的 JSON 输出字段：

| 事件 | 何时运行 | 能做什么 |
| --- | --- | --- |
| `PreToolUse` | 工具调用准备之前 | 拒绝、要求询问你、放行（仅限你自己的 Hook）、改写参数（`updatedInput`）、补充上下文 |
| `PostToolUse` | 工具调用成功之后 | 在结果后给模型理由（`decision: "block"`）或补充上下文 |
| `PostToolUseFailure` | 工具准备或执行失败之后，包括准备时发现文件缺失 | 在结果后补充上下文 |
| `UserPromptSubmit` | 模型读取你发送的提示之前 | 拦下该提示（不进入对话，并显示原因）、补充上下文 |
| `SessionStart` | 新对话（`startup`）、继续对话（`resume`）、`/clear`（`clear`）、压缩之后（`compact`） | 补充上下文 |
| `SessionEnd` | Agent 结束时：在 Agents 总览中停止，或其进程退出（如 `tickloop daemon stop`） | 只做通知 |
| `Stop`、`SubagentStop` | Agent 或 Subagent（交回结果之前）即将结束本轮时 | 拦下，让本轮按给出的理由及 `additionalContext` 继续；下次输入带 `stop_hook_active: true` |
| `StopFailure` | 一轮因模型错误结束时 | 只做通知 |
| `PreCompact` | `/compact`（`manual`）或自动压缩（`auto`）之前 | 拦下，取消这次压缩 |

`SessionStart` 完成后才运行首个提示的 `UserPromptSubmit`。等待这些 Hook 时，提示保留在队列；先记录会话上下文，再记录提示及其提示 Hook 上下文。仍在等待 Hook 的提示会跨 Worker 重启或计划升级保留。等待中按 `esc` 会将提示留在队列，Continue 或发送另一条提示后继续；`/stop` 会丢弃排队的提示。

- matcher 接受精确名、`|` 分隔的列表或正则表达式，空或 `*` 匹配全部，用于工具类事件、`SessionStart`（来源）和 `PreCompact`（触发方式）。工具名用 Tickloop 的名字：`Read`、`Write`、`Edit`、`Bash` 与 `mcp__*` 与 Claude Code 相同，`Agent` 或 `Task` 也匹配 `Subagent`。
- 字段：`command`、`timeout`（秒，默认 600）、`if`、`async`，以及带 `rewakeMessage`、`rewakeSummary` 的 `asyncRewake`。`if` 写一条权限规则，如 `Bash(git push*)`，只在该规则可能匹配的调用上运行；重定向、管道和 `&&` 会正常拆分，任一子命令匹配即运行；命令替换等判断不了的情况照常运行。
- 输入含 `session_id`（Chat 的 ID）、`transcript_path`（Claude Code 格式的对话副本，读转录的脚本可以照常使用）、`cwd`、`permission_mode`、`hook_event_name`，以及各事件自己的字段。命令在工作目录中运行，并设有 `CLAUDE_PROJECT_DIR`；插件 Hook 另有 `TICKLOOP_PLUGIN_ROOT`、`TICKLOOP_PLUGIN_DATA` 及其 `CLAUDE_` 别名，命令中的 `${CLAUDE_PLUGIN_ROOT}`、`${user_config.KEY}` 会被替换。插件 Hook 引用的 `user_config` 值缺失时不运行，等待配置。引用的敏感值只交给执行进程，界面命令、输出、错误、上下文、改写参数与运行记录会脱敏。
- 退出码 0 表示成功：stdout 是 JSON 时按 JSON 解析，否则在 `UserPromptSubmit` 与 `SessionStart` 中作为上下文。退出码 2 表示拦下，stderr 交给模型（或拒绝该提示、让本轮继续）。其他退出码默认报告非阻断错误；若 stdout 是有效 JSON，其支持的决定、输入改写与上下文仍生效。超时为非阻断错误。
- 匹配到的 Hook 并行运行；决定不一致时 deny 优先于 ask，ask 优先于 allow。`ask` 在 Auto 下也会问你。你自己的 Hook 返回 `allow` 时跳过审批提示与 Auto 审查，显式的 Ask、Deny 规则、敏感路径检查与 Plan 照常生效；执行环境或工具本身要求的确认与独立审查也保留。`updatedInput` 之后，审批界面显示实际将运行的参数。
- `async: true` 让 Hook 在后台运行、不阻塞 Agent；结束后显示结果，其 `systemMessage` 与 `additionalContext` 随下一次请求交给模型。`asyncRewake: true` 同样在后台运行，退出码 2 时以 Hook 的输出唤醒 Agent，就像后台命令完成时一样。运行中的 Hook 以第一行输出 `{"async": true}` 时同样转入后台（与 Claude Code 一致），之后按 `async` Hook 处理，超时仍从开始运行时算起。后台 Hook 的决定类输出一律忽略。
- `esc` 和 `/stop` 取消当前步骤正在等待的 Hook；这样被取消的 `PreToolUse` Hook 也会取消对应的工具调用，模型读到该调用未执行。后台 Hook 与 `StopFailure` Hook 不随 `esc` 取消，`/stop` 与停止 Agent 时结束。后台 `SessionEnd` Hook 仍在 Agent 结束时运行：Agent 与其他 `SessionEnd` Hook 一起等它，最多 60 秒，其输出无人读取。持续拦下的 `Stop` Hook 可用 `esc` 或 `/stop` 结束循环。与 Claude Code 一致，`Stop`、`SubagentStop` Hook 在中间没有工具调用时最多连续拦下 8 次，第 9 次拦截被忽略并给出警告，本轮结束；模型调用交回以外的工具后重新计数；交回本身不重置计数，`stop_hook_active` 也不会因此复位。
- ACP 与 Print 收束时会取消该 Chat 的后台 Hook（包括运行中转入后台的 Hook），最多等待 10 秒。超时或命令结果未知时收束失败，不重发命令；被取消的 Hook 不再投递迟到通知。其他 Chat 与普通终端会话的 Hook 生命周期不变。
- Hook 的 `systemMessage`、拦下的原因和失败会在对话中显示一行。每次运行（来源、事件、耗时、退出码、决定与截断后的输出）都列在 `/permissions` → Recent activity 中（每个对话最近 100 条）。

Hook 以你的身份在沙箱之外运行，与 Claude Code 中相同。

**插件和仓库的 Hook 不能放行任何操作。** 它们的 `allow` 记为已忽略，调用按它们未作答处理；它们仍可以拒绝、要求询问你、改写参数和补充上下文。插件或仓库的 Hook 改写了某次调用的参数时，你自己的 Hook 给出的 `allow` 对这次调用不再有效。

暂不支持，各自给出警告、其余 Hook 照常运行：`prompt`、`agent`、`http`、`mcp_tool` 类型；`args`、`shell`、`once`、`onFailure` 字段；`Notification`、`PermissionRequest`、`UserPromptExpansion`、`FileChanged` 等其他事件；`CLAUDE_ENV_FILE`；`disableAllHooks`；Skill 或 agent frontmatter 中的 Hook；Codex 的 `.codex/hooks.json`。已知限制：

- `UserPromptSubmit` 读取你实际发送的文字（包括附带文件等上下文之后的提示），也对工作时排队、在本轮中读入的提示运行；不读取系统附加上下文，不对定时任务、Tick 更新或 Agent 消息运行。
- `PostToolUseFailure` 不用于权限拒绝、参数 schema 校验失败或 `PreToolUse` 自身拒绝、取消；这些情况工具未执行。
- `tool_response` 是工具的结果文本（是 JSON 时按 JSON 解析），不是 Claude Code 按工具区分的结构；转录副本中的工具结果也只保留文本。
- `asyncRewake` Hook 在 `esc` 之后才以退出码 2 结束时，通知留到下一轮，不唤醒 Agent；`/stop` 会取消后台 Hook，并丢弃旧运行的迟到输出。
