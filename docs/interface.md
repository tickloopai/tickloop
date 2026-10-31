# Interface

[Tickloop](../README.md) · English | [简体中文](zh-CN/interface.md)

## Keys

- `enter` sends a message. While the agent works it waits for the next step; `ctrl+enter` interrupts and sends right away.
- `esc` interrupts the agent. Press it twice on an empty input to undo to an earlier message.
- `shift+tab` switches between Auto and Plan mode.
- `ctrl+j` inserts a new line; `ctrl+g` edits your message in an external editor.
- In lists and menus, use `ctrl+n`/`ctrl+p` or the arrow keys, `enter` to choose and `esc` to go back.

### Extension Tick cards

Extension Ticks appear as cards in the Monitor column, inside Herdr or any other terminal.

- In the Monitor list, `space` (or a click on the row) shows or hides a Tick's card; `enter` (or a click on an open card) steps into the Tick, so your keys go to it, `ctrl+c` included. Several cards can be open at once.
- `tab` and `shift+tab` move through every row in turn and preview the card of each Tick they pass. The preview closes when the selection moves on or you leave the list, unless you step into it (`space` hides it, as on any card).
- Other moves only select a row, but an open card squeezed out for lack of room shows while its row is selected.
- When rows with an update you have not seen are scrolled out of view, a `↑ 2 updated` or `↓ 1 updated` line says so, and clicking it jumps to the nearest one.
- Rows stay in the order they joined, so an update never moves them.
- Inside a card, a single `esc` goes to the Tick; `esc esc` steps back to the list and the card stays open.
- Ticks that support it hand back keys they do not use: then `esc` alone steps out, the up and down arrows step out to the list with the card above or below selected, `tab` and `shift+tab` step out and move through the list as they do there, and left goes back to your input.
- Stepping into another card still takes `enter` or a click.
- Clicking outside the card also steps out, and the click then does what it does there: on another Tick's row it shows that Tick's card.
- The program keeps running when you hide its card or step out.
- When the agent uses a Tick or reads its update, the Tick's card opens by itself, unless you hid it.
- When a Tick ends, its card keeps the last screen and its rule says how, such as `exited 101` or `ended`.

### Terminal Ticks and Shells

Terminal Ticks and Shells have no card.

- `enter` (or a double click on the row) shows one full screen in your terminal.
- The last line lists the keys, and you can click the underlined ones to hide or reopen.
- While the program does not use the mouse itself, drag to select text and it is copied when you let go; in a program that uses the mouse, hold Shift to select.
- A terminal Tick gets every key there, `ctrl+c` included; your first `esc` goes to it too, and a second `esc` within a second hides it.
- A Shell's screen is read only: `esc` hides it, the arrows, `ctrl+n`/`ctrl+p` and page up/down scroll back through its output, `G` or end follows the newest output again, and `ctrl+c` stops it.
- Permission requests wait until you hide the screen, and the terminal rings once. The program keeps running when you hide it.
- When a terminal Tick's program exits, its last screen stays with how it ended on the bottom line: `esc` hides it and `enter` starts it again in the same row.
- A Shell that ended stays shown too; to run its command again, type `!command`.

### tickloop attach

`tickloop attach` shows a Tick or a Shell that a running agent opened, full screen in any terminal: it lists them to choose from, or takes one such as `tickloop attach hosted:…`. Hiding it ends the command and leaves the Tick running.

### Agents overview

- `←` on an empty input opens the Agents overview.
- As in Claude Code's agent view, Agents are grouped under Needs input (with how long each has waited), Working and Completed.
- Completed holds Idle, Completed, Failed and Stopped Agents, newest end first, and folds what the screen cannot hold behind `… N more`.
- An Agent that holds nothing is not listed.
- Agents whose process ended stay listed until `cleanupPeriodDays` (30 days by default) passes, and nothing restarts them.
- One whose turn had finished with nothing pending stays Completed, and one that ended on a question or plan for you stays in Needs input.
- One that was working, or idle with work still pending (a Goal, `/loop` or scheduled task, a Tick Subscription, a background Shell or Subagent), shows Failed when the daemon stopped within 48 hours of its last activity and Stopped when it was longer.
- `enter` on such a row loads its saved conversation without running anything, and your next message continues it; a row that ended while the daemon was off asks for a second `enter`.
- `space` on a row opens its peek panel: the Agent's state, its latest sentence and what it waits on, with a reply box below.
- `enter` sends the reply without leaving the overview (a number answers the Agent's question when every option is on screen; a permission request or plan approval is answered only in the Agent's own view; replying to an ended row loads it first).
- `↑`/`↓` move the panel to the next Agent, `→` opens the Agent and `esc` (or `space` on an empty reply) closes the panel.
- `enter` or `→` folds a group heading.
- `x` twice on a running Agent stops it (it stays listed as Stopped); `x` twice on an ended row removes the row and keeps the conversation.
- `r` (or `/resume`) in the overview lists the saved conversations not in the list yet and adds the one you choose.

### Closing a row

To close/remove a Tick, stop a Shell, unlink an Artifact or delete a scheduled task in Monitor, press plain `x` twice on the same row within three seconds.

- The first press shows a red `Press x again to …` hint, with no dialog.
- Expiry, selection movement, another key or leaving cancels it.
- `Ctrl+X` does not perform these actions; in text inputs and filters, `x` stays text.

## Context

When a conversation nears the end of the model's context window, the agent summarizes it before its next request. That happens once the context passes the window less room for the summary (the model's output limit, at most 20,000 tokens) and a 13,000-token buffer, but never below half the window; `/context` shows the space past that point as the Autocompact buffer.

From 20,000 tokens before that point the status line's `Context N% used` turns to the warning color and the conversation says once how near it is; `/compact` compacts at any time. To compact earlier, set `autoCompactWindow` in `settings.json` to a number of tokens, for example `"autoCompactWindow": 400000`; when it is smaller than the model's window, the point is measured in it instead. Conversations started or reopened afterwards use it.
