# 界面

[Tickloop](../../README.zh-CN.md) · [English](../interface.md) | 简体中文

## 按键

- `enter` 发送消息。Agent 工作时消息会等到下一步再处理；`ctrl+enter` 打断并立即发送。
- `esc` 打断 Agent。输入为空时连按两次，可撤回到之前的某条消息。
- `shift+tab` 在 Auto 与 Plan 模式之间切换。
- `ctrl+j` 换行；`ctrl+g` 用外部编辑器编辑消息。
- 列表和菜单中用 `ctrl+n`／`ctrl+p` 或方向键移动，`enter` 选择，`esc` 返回。

### 扩展 Tick 卡片

扩展 Tick 以卡片显示在 Monitor 列中，在 Herdr 内外都一样。

- 在 Monitor 列表中按 `space`（或点击该行）显示或隐藏 Tick 的卡片；按 `enter`（或点击已打开的卡片）进入 Tick，之后按键都交给它，`ctrl+c` 也一样。可以同时打开多张卡片。
- `tab` 和 `shift+tab` 依次经过每一行，并预览经过的 Tick 的卡片。选中移开或离开列表时预览关闭，除非进入了它（按 `space` 与其他卡片一样隐藏它）。
- 其他移动方式只选中行，但已打开、因空间不够被折叠的卡片在其行被选中时会显示出来。
- 有未看更新的行滚出视野时，边缘会显示 `↑ 2 updated` 或 `↓ 1 updated`，点击它跳到最近的一个。
- 各行保持加入时的顺序，更新不会让它们移动。
- 在卡片中单按 `esc` 交给 Tick；连按 `esc esc` 离开，卡片保持打开。
- 支持的 Tick 会把自己不用的按键交还：这时单按 `esc` 即离开，上下方向键离开到列表并选中上一张或下一张卡片的行，`tab` 和 `shift+tab` 离开到列表并像在列表中一样移动，左方向键回到输入框。
- 进入另一张卡片仍需按 `enter` 或点击。
- 点击卡片以外的地方也会离开，这次点击接着按那里的规则生效：点在另一个 Tick 的行上会显示那张卡片。
- 隐藏卡片或离开都不会结束程序。
- Agent 使用某个 Tick 或读取它的更新时，它的卡片会自动打开，除非你隐藏过它。
- Tick 结束后，卡片保留最后的画面，rule 上写明结束方式，例如 `exited 101` 或 `ended`。

### Terminal Tick 与 Shell

Terminal Tick 与 Shell 没有卡片。

- 按 `enter`（或双击该行）在当前终端全屏显示。
- 最后一行列出按键，带下划线的可以点击隐藏或重开。
- 程序本身不用鼠标时，拖动选中文字，松开即复制；程序自己用鼠标时，按住 Shift 拖选。
- Terminal Tick 在全屏中收到所有按键，包括 `ctrl+c`；第一次 `esc` 也交给它，一秒内再按一次 `esc` 隐藏。
- Shell 的全屏只读：`esc` 隐藏，方向键、`ctrl+n`／`ctrl+p` 和上下翻页回看输出，`G` 或 End 回到最新输出，`ctrl+c` 停止它。
- 权限请求会等到隐藏后再显示，终端响一次。隐藏不会结束程序。
- Terminal Tick 的程序退出后，最后的画面留着，底行写明结束方式：`esc` 隐藏，`enter` 在同一行重新启动它。
- Shell 结束后画面也留着；要再跑一次，在输入框输入 `!命令`。

### tickloop attach

`tickloop attach` 在任意终端全屏显示运行中的 Agent 打开的 Tick 或 Shell：不带参数时列出供选择，也可直接指定，例如 `tickloop attach hosted:…`。隐藏后命令退出，Tick 继续运行。

### Agents 总览

- 输入为空时按 `←` 打开 Agents 总览。
- 和 Claude Code 的 agent view 一样，Agent 分为 Needs input（显示已等待多久）、Working 和 Completed 三组。
- Completed 组包含 Idle、Completed、Failed 和 Stopped 状态的 Agent，最近结束的在前，屏幕放不下的折叠成 `… N more`。
- 没有任何内容的 Agent 不列出。
- 进程已结束的 Agent 会一直列出，直到超过 `cleanupPeriodDays`（默认 30 天），不会自动重启。
- 回合已结束、没有待办工作的仍显示为 Completed；结束时在等你回答问题或审阅计划的留在 Needs input。
- 正在工作、或空闲但还有待办工作（Goal、`/loop` 或定时任务、Tick 订阅、后台 Shell 或子代理）的，daemon 停止时距最后活动不到 48 小时显示为 Failed，超过的显示为 Stopped。
- 在这样的行上按 `enter` 只加载保存的对话、不运行任何东西，发出下一条消息才继续；在 daemon 关闭期间结束的行需要再按一次 `enter`。
- 在行上按 `space` 打开 peek 面板：显示该 Agent 的状态、最新一句话和它在等的请求，下方是回复框。
- `enter` 发送回复且不离开总览（选项全部显示在屏幕上时，输入数字可回答它的问题；权限请求和计划审批只能在该 Agent 自己的界面里处理；回复已结束的行会先加载它）。
- `↑`／`↓` 把面板切到相邻的 Agent，`→` 打开该 Agent，`esc`（或回复框为空时按 `space`）关闭面板。
- 分组标题用 `enter` 或 `→` 折叠。
- 在运行中的 Agent 上按两次 `x` 停止它（它以 Stopped 留在列表里）；在已结束的行上按两次 `x` 移除该行，对话仍保留。
- 在总览中按 `r`（或输入 `/resume`）列出尚未在列表中的已保存对话，选中的会加入列表。

### 关闭一行

在 Monitor 中关闭或移除 Tick、停止 Shell、解除 Artifact 关联或删除定时任务：在同一行三秒内按两次普通 `x`。

- 第一次只在按键提示行显示红色 `Press x again to …`，没有确认弹窗。
- 超时、移动选择、按其他键或离开会取消。
- `Ctrl+X` 不执行这些操作；文本输入和筛选框中的 `x` 仍是文字。

## 上下文

对话接近模型的上下文窗口上限时，Agent 会在下一次请求前先把对话压缩成摘要。触发点是窗口减去摘要所需的输出空间（模型的输出上限，最多 20,000 tokens）再减去 13,000 tokens 的缓冲，但不低于窗口的一半；触发点之后的空间就是 `/context` 中显示的 Autocompact buffer。

距触发点不到 20,000 tokens 时，状态行的 `Context N% used` 改为警示色，对话中也会提示一次；随时可以用 `/compact` 手动压缩。想更早压缩，可在 `settings.json` 中把 `autoCompactWindow` 设为 token 数，例如 `"autoCompactWindow": 400000`；它小于模型窗口时，触发点改按它计算。此后新建或重新打开的对话生效。
