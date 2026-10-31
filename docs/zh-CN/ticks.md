# Tick

[Tickloop](../../README.zh-CN.md) · [English](../ticks.md) | 简体中文

Tick 是 Agent 可以打开、盯着并操作的实时程序，比如行情源或文档阅读器。Tick 和插件一样安装，共用 marketplace 和存储，但单独管理：用 `/ticks` 查找、安装和登录 Tick，或在命令行中操作：

```sh
tickloop tick list --available
tickloop tick install NAME                # 或 NAME@MARKETPLACE
tickloop tick list
tickloop tick login NAME                  # 数据源需要登录时使用
tickloop tick logout NAME
tickloop tick update NAME                 # 省略 NAME 更新所有 Tick
tickloop tick disable NAME
tickloop tick enable NAME
tickloop tick uninstall NAME              # 删除其数据，保留登录
tickloop tick uninstall NAME --keep-data  # 同时保留数据
tickloop tick install --path ./my-tick    # 本地 Tick 目录或 .tar.gz 压缩包
```

官方 Tick 来自内置的 `tickloop` marketplace，源码在 [tickloopai/ticks](https://github.com/tickloopai/ticks)。带 Tick 程序的插件就是 Tick，即使它也带 Skill：装好后只出现在 `/ticks` 和 `tickloop tick` 中，不出现在 `/plugins`。安装前就能看出种类时（Tick marketplace 的包或本地目录），`tickloop plugin` 拒绝 Tick 并给出应改用的 `tickloop tick` 命令，反之亦然。

编写 Tick 见 [编写 Tick](writing-ticks.md)。

## /ticks 面板

在 `/ticks` 中，用 `Tab` 或左右键切换 **Installed**（已安装）和 **Available**（可安装），`Ctrl+N`/`Ctrl+P` 或上下键选择。已安装页按 `Enter` 打开 Tick 的操作：登录/退出登录、更新、启用/禁用；每行显示 marketplace、启用状态、版本和登录状态。登录在面板内完成：OAuth 的 Tick 显示链接和验证码，在任意设备的浏览器中确认；密钥以遮掩方式输入。可安装页按 `Enter` 安装或更新所选 Tick，按 `/` 开始过滤名字、说明和 marketplace，不区分大小写；过滤时字母是文字，`Ctrl+N`/`Ctrl+P` 选择，`Ctrl+G` 打开外部编辑器。`Esc` 退出过滤并保留查询，再按一次关闭面板。

在已安装页同一行三秒内按两次 `x` 卸载：删除该 Tick 的数据，保留登录，不停止运行中的 Tick。移动选择、按其他键、离开或超时都会取消确认。登录保留在配置目录的 `credentials/NAME/`；要删除保存的登录，先退出登录再卸载。

装好 Tick（需要登录的先登录）后，让 Agent 按名字打开它，例如"打开 NAME 这个 Tick 并盯着它"。安装不运行任何程序：Tick 程序只在按你的权限设置打开时以你的身份运行。禁用的 Tick 不能按名字打开。安装、更新、卸载和登录由你发起，Agent 只会请你去做。
