# 权限

[Tickloop](../../README.zh-CN.md) · [English](../permissions.md) | 简体中文

执行模式决定 Agent 如何获得操作许可。macOS 上 Bash 与 Shell 的沙箱在另一页：[沙箱](sandbox.md)。

## 执行模式

**Auto**（默认）由审查模型批准常规操作，有风险的才问你；**Ask** 允许读取和编辑工作目录内的文件，其他受保护操作逐次询问；**Plan** 只做计划；**Full Access** 跳过普通审批，但显式 Ask 与 Deny 规则仍然生效。可用 `/permissions`、`/mode` 或 `shift+tab` 切换，用 `--mode 模式` 启动，或在 `settings.json` 中用 `defaultMode` 设置新 Chat 的初始模式。

`--permission-mode` 是 `--mode` 的别名；两者同时给出时必须指向同一模式。两个参数与 `defaultMode` 都接受 Claude Code 的取值：

| 取值 | 在 Tickloop 中 |
| --- | --- |
| `acceptEdits`（或 `ask`） | Ask |
| `auto` | Auto |
| `plan` | Plan |
| `fullAccess` | Full Access |
| `default` | Ask，并警告改用 `ask` |
| `dontAsk` | 不是模式：改用 `--deny-on-prompt`，凡需你批准的操作一律拒绝 |
| `bypassPermissions` | 不是模式：改用 `--dangerously-skip-permissions`，需要外部隔离 |

启动参数的取值不可用时拒绝启动，并说明替代写法；同样的值写在 `defaultMode` 中只给出同样的警告，新 Chat 以 Auto 启动。

Plan 模式仍拒绝文件编辑和会改变状态的命令，只有 Agent 写的计划文件例外；该文件在任何模式下都可直接读写、不需询问。Agent 提交计划后，批准选项显示进入 Plan 前的模式，确认后以该模式开始；也可附反馈继续规划，在反馈输入框按 `shift+tab` 则连同反馈批准。Agent 在没有计划时请求离开 Plan，回答“是”回到进入 Plan 前的模式，回答“否”继续计划。无论哪种情况，若原模式未知（如重启后或从 Plan 模式启动的对话），都回到 Ask。原本要问你的操作，如 `WebFetch`、读取工作目录外的文件或调用 MCP 工具，按进入 Plan 之前的同一模式处理，无论是你切到 Plan 还是 Agent 进入的：从 Auto 进入时与 Auto 相同，Auto 会直接允许的就直接允许，其余交给 Auto 审查，审查不放行时才问你；从 Full Access 进入时直接允许、不再询问，与 Claude Code 在可用 bypass 权限时的 Plan 模式一样，但文件编辑和会改变状态的命令仍然拒绝；从 Ask 进入时，或原模式未知时，则询问你。显式 Ask 与 Deny 规则仍然优先，宽泛的 Shell Allow 规则与 Auto 下一样不能跳过审查。若从 Auto 进入后也想改为询问，在 `settings.json` 中设置 `"useAutoModeDuringPlan": false`，此后新建或重新打开的对话生效。

## 单次启动的规则

为新 Agent 添加临时工具规则，可运行 `tickloop --allowedTools "Bash(git:*) Edit" --disallowedTools "Bash(rm:*)"`，也支持别名 `--allowed-tools` 和 `--disallowed-tools`。参数可重复，规则以空格或逗号分隔；无法解析的规则会让启动失败。这些参数不能与 `--continue`、`--resume` 或 `--chat-id` 同用。`/permissions` 中这些规则标为只读的 **Launch arguments（启动参数）**，只作用于该 Agent 及其 Subagent，重启后失效，不写入设置，也不影响其他 Agent。允许的工具是追加到现有规则上的，不是排他的白名单：未列出的工具照常处理。Deny、Ask 和敏感路径检查仍优先；宽泛的 Bash allow 仍需 Auto 审查。加上 `--deny-on-prompt`，需要人工确认的操作将直接拒绝。

## 附加目录

用 `tickloop --add-dir ../shared --add-dir /path/to/docs` 为这次启动创建的 Agent 添加目录，或在 Chat 中使用 `/add-dir PATH`。要让新 Agent 默认包含这些目录，在用户自己的 `settings.json` 中写 `"permissions": {"additionalDirectories": ["../shared"]}`；同一字段也可放在 `projects["/项目绝对路径"]` 下，仅对这个工作目录精确生效，并与全局列表合并。相对路径按启动工作目录（指定时为 `--cwd`）解析，不可用的目录会警告并跳过。

这些来源合并到 Chat 现有的附加目录中并去重，支持读取、编辑和 `@` 文件提及；敏感文件检查和显式 Ask/Deny 规则仍然生效。`/cwd` 会显示每个目录的来源。仓库设置不能添加目录，`--add-dir` 与 `/add-dir` 也不会把目录写入设置。

## 从 Claude Code 导入 Auto 规则

在 `/permissions` → Auto mode 中选择 **Import from Claude Code**，可把 Claude Code 用户设置（`~/.claude/settings.json`；设置了 `CLAUDE_CONFIG_DIR` 时为该目录中的 `settings.json`）里 `autoMode` 的规则一次性复制过来。不读取任何项目文件，也不修改 Claude Code 的文件。`allow` 条目成为 Soft allow 规则，`soft_deny`（或旧名 `deny`）成为 Soft deny 规则，`environment` 成为标明“导入”来源的全局 Environment 条目。预览按“将添加 / 将跳过”列出每条及原因：与已有自定义规则文字相同的、与 Tickloop 内置规则同名的默认跳过，可以手动勾回；空的、超过 4096 字节的，或会让某组超过 256 条的条目不能保存。Soft allow 条目标有 **Will relax review**（会放宽审查）。`Ctrl+N`/`Ctrl+P` 移动，空格勾选，`enter` 保存，`esc` 取消且不改动。内置规则会保留；导入后的规则就是普通的自定义规则，不能覆盖显式 Ask 或 Deny。只有你能发起导入，Agent 没有对应的工具。
