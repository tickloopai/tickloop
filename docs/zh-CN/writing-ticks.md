# 编写 Tick

[Tickloop](../../README.zh-CN.md) · [English](../writing-ticks.md) | 简体中文

程序实现 [Tick 协议](../../tick/protocol/SPECIFICATION.md) 就能成为扩展 Tick；可从 [Counter 示例](../../tick/examples/counter.py) 开始。Tick 显示在至少 30 列宽的 Monitor 卡片中：要在这个宽度下可用，尺寸变化后重画整个屏幕，并可在描述中用 `screen.min_rows` 要求 6 到 14 行（默认 6），用 `screen.max_rows` 在 Monitor 有空间时要更多行。Tick 不应使用 `esc`：返回键是 `backspace` 而不是 `esc`，并且绝不能依赖连按两次 `esc`，那会离开卡片。Tick 应把自己不用的按键交还：启动时写一次 `\x1b]7777;tickloop;bubble=on\x07`，每个不用的按键写 `\x1b]7777;tickloop;key=<name>\x07`（`esc`、`up`、`down`、`left`、`right`、`tab`、`backtab`），详见协议规范。例如 Neovim：

```lua
vim.api.nvim_create_autocmd('VimEnter', {
  callback = function() io.stdout:write('\27]7777;tickloop;bubble=on\7') end,
})
vim.keymap.set('n', '<Esc>', function() io.stdout:write('\27]7777;tickloop;key=esc\7') end)
```

在 Monitor 和命令行里使用已安装的 Tick，见 [Tick](ticks.md)。
