# Writing Ticks

[Tickloop](../README.md) · English | [简体中文](zh-CN/writing-ticks.md)

Any program can become an Extension Tick by speaking the [Tick protocol](../tick/protocol/SPECIFICATION.md); start from the [counter example](../tick/examples/counter.py). A Tick is shown in a Monitor card at least 30 columns wide: it must stay usable at that width, redraw its whole screen when resized, and can ask for 6 to 14 rows with `screen.min_rows` in its description (6 by default) and for more when the Monitor has room with `screen.max_rows`. A Tick should not use `esc`: its back key is `backspace` instead of `esc`, and it must never need a double `esc`, which steps out of the card. It should hand back the keys it does not use by writing `\x1b]7777;tickloop;bubble=on\x07` once at startup and `\x1b]7777;tickloop;key=<name>\x07` (`esc`, `up`, `down`, `left`, `right`, `tab`, `backtab`) for each unused key; see the specification. In Neovim, for example:

```lua
vim.api.nvim_create_autocmd('VimEnter', {
  callback = function() io.stdout:write('\27]7777;tickloop;bubble=on\7') end,
})
vim.keymap.set('n', '<Esc>', function() io.stdout:write('\27]7777;tickloop;key=esc\7') end)
```

Using an installed Tick from the Monitor and the command line is described in [Ticks](ticks.md).
