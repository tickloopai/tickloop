---
name: tickloop
description: Show stock prices, quotes or K-line charts in Ticks, operate a Tick, or subscribe to updates. Use for 看看特斯拉股价 and 打开K线, even without the word Tick. Check the host first; Herdr is supported, tmux and Zellij are not yet integrated.
---

# Tick Loop Workspace

## 1. Select the workspace host

Start with `workspace_host` in Current host state. The program supplies `name`, `integration`, and `tick_executable`; an installed multiplexer command or a repository name is not proof of the current environment.

| Current host | Action |
| --- | --- |
| `herdr`, integration `available` | Continue to the Herdr instructions below. Confirm live access with `ReadWorkspace({})`. |
| `herdr`, integration `unavailable`, or a failed host connection | Explain that this Agent is not connected to Herdr and cannot operate Ticks. Report the connection diagnostic. Stop this workflow. |
| `tmux` | Say that tmux was detected but Tick Loop's tmux integration is not implemented yet. Cross-Tick operations are unavailable; stop this workflow. |
| `zellij` | Say that Zellij was detected but Tick Loop's Zellij integration is not implemented yet. Cross-Tick operations are unavailable; stop this workflow. |
| `standalone` | Say that no supported workspace host is connected, so cross-Tick operations are unavailable. Stop this workflow. |

For an unsupported branch, briefly name the detected host and explain that its Tick integration is unavailable, then end the turn. Wait for a new user request before troubleshooting installation or switching to another task. `tick_executable` is only evaluated for supported Herdr; its null value in other hosts does not mean the executable is missing. Explain the status in ordinary language, without dumping internal context fields.

Ordinary questions remain available in these environments. Do not simulate a successful Tick operation or bypass the missing integration through shell multiplexer commands. If host metadata is missing, report that the environment cannot be confirmed rather than assuming Herdr. Nested environment markers are resolved by the program in Herdr, tmux, Zellij order.

## 2. Herdr: choose the requested operation

Use the installed host tools as the authority for targets and capabilities. Herdr owns Workspace → Tab → Pane; a Tick Instance identifies one running program independently of its Pane. Default to the Agent's Work Context Workspace, which stays independent of the View used to inspect it. When the user explicitly selects another Workspace, include its `workspace_id` in discovery and subsequent operations. Without a default Workspace, `ReadWorkspace({})` discovers available locations; choose an explicit Workspace for creation. An Agent can discover and operate explicit targets while it has no View.

“看看特斯拉股价” requests a visible quote Tick even though the user did not say Tick. Prefer an existing matching view when the user asks to see information; open the requested view if none exists. An explicit request to open a new Tick creates a new instance. A request to see a price uses Quote; a request for K线 or a chart uses Kline. A request explicitly asking for text only can use an ordinary data lookup instead.

Select existing views from inventory titles and capabilities, then read relevant candidates to confirm the symbol and view. Inspect candidate content only when it helps identify the requested target. Opening, discovering, or reading a Tick does not establish a subscription; use the subscription workflow only for continued observation.

### Choose where the Tick appears

`ReadWorkspace` returns an optional `caller` for the View carrying this interaction. “这个 Tab”, “你所在的 Tab”, and “在你旁边” refer to that interaction location, independent of other clients' focus. The tool freezes this source for the accepted input; a background Trigger or task continuation may have no interaction source. A missing or changed source affects relative placement only: discover locations and use explicit targets within the existing task and authorization. For `ModifyTick`, select `placement` from the request:

| Requested location | `placement` |
| --- | --- |
| Beside the View carrying this interaction, with no direction specified | `{"type":"current_tab","direction":"right"}` |
| Left, right, above, or below this Agent | `{"type":"current_tab","direction":DIRECTION}` |
| Left, right, above, or below a specific Pane in the selected Workspace | `{"type":"pane","pane_id":PANE_ID,"direction":DIRECTION}` |
| New Tab | `{"type":"new_tab"}` for `open`; also the default when `open` omits placement |

Set `DIRECTION` to `left`, `right`, `up`, or `down` as requested; omitted direction defaults to `right`. For a new Tick, pass the placement to `action: "open"`. If the Tick is already running and the user wants it at a specific location, use `action: "move"` with its complete `tick_instance_id` and the desired placement. An explicit relative direction requires a move even when the Tick is already in the target Tab; sharing a Tab does not establish which side of the anchor Pane it occupies. For a request only to be in the current Tab, report an existing instance's location if it is already there. Moving preserves the process, instance, and subscriptions; check the placement result, then read back the instance and verify its Tab before confirming. Right/down do not request focus; if the moved Pane was focused, Herdr chooses a remaining Pane. Left/up finish focused on the anchor Pane (the Agent for `current_tab`, the named Pane for `pane`). Destination and source must be in the selected Workspace; moving between Workspaces is not exposed by this tool.

For example, “就在这个tab里打开吧” after opening a news Tick means move that existing instance with `{"action":"move","tick_instance_id":INSTANCE,"placement":{"type":"current_tab","direction":"right"}}`. “放在你左边” uses `direction: "left"` even if that Tick is already in this Tab. A new Kline request with the same location uses `action: "open"`, the Kline command, and that placement. Use these host tools directly for placement.

### Open a bundled Tick

Use `workspace_host.tick_executable` as the executable. It identifies the `tick` installed beside the Agent, or the executable found on PATH. If it is null, explain that the companion `tick` is missing and needs to be built or installed; `make build` and `make install` include both binaries. This is a missing executable, not a reason to search project source for a launcher.

Call `ModifyTick` with `action: "open"`, a `command` argv array and optional `cwd`. Omitted `cwd` uses the Agent's file work directory, independently of its View position. With no View, choose an explicit Workspace and `new_tab` or a named Pane for placement. Build the argv from the requested view:

| Requested view | Arguments after the executable |
| --- | --- |
| Stock price / 行情 / 报价 | `quote SYMBOL` |
| K-line / K线 / candlestick chart | `kline SYMBOL` |
| Explicit offline chart demonstration | `demo kline` |

Use the broker symbol format: Tesla / 特斯拉 is `TSLA.US`, Apple / 苹果 is `AAPL.US`, and Tencent / 腾讯 is `700.HK`. Clarify an ambiguous company or market before launching. For example, “打开特斯拉K线” means `command: [TICK_EXECUTABLE, "kline", "TSLA.US"]`. Omitted chart period uses the command's default; specify `--period` when the user asks for a period. Use the executable's `--help` or `kline --help` for other options and Tick types. A real stock request uses real data; the demo is a separate, explicitly requested workflow.

Bundled interactive Ticks connect automatically in Herdr. External protocol programs use `command: [TICK_EXECUTABLE, "run", "--", PROGRAM, ...ARGS]`. Ordinary programs can run directly. The host passes the Agent's Tick configuration and terminal locale when opening a Pane. Report the returned host, Workspace, Tab, Pane, Tick Instance and state that were actually verified, so the user can find it. Requested left/right placement describes intent; partial or initializing results may still have unknown fields.

The host checks local prerequisites before launching a bundled Tick. If authorization is missing, report the diagnostic and the authorization command (`TICK_EXECUTABLE auth`); an authorization failure is not a successful launch. Do not read credential files. After launch, read the returned instance to confirm that the requested symbol and view are actually running. Report success only after that check. An `initializing` or partial-failure response means some steps may already have run: inspect the existing instance and its location before retrying placement. Do not create a duplicate Tick. An `OutcomeUnknown` mutation may already have executed; inspect its state instead of resending it.

### Read or operate a running Tick

Keep the complete opaque `tick_instance_id`; focus and shortened Pane IDs do not select a Tick. `ReadTick` returns structured content and a tool catalog for protocol Ticks, or recent text for ordinary programs. After reading the catalog, call `CallTickTool` with the declared `tool` and `args` matching its schema. Query tools have `mutates: false`; actions have `mutates: true`. Existing execution modes and permission rules govern the resolved action. Verify changes by reading the same instance again.

For ordinary programs, use `ModifyTick` with `action: "send_input"`, the selected instance and `text` or logical `keys`. Herdr checks the running program before sending, but cannot atomically compare its identity and send input. Report identity uncertainty and select the target again when needed. `action: "close"` closes the selected Pane and exits its Tick. A changed target or tool catalog requires fresh preparation and permission.

## 3. Keep a Loop informed

For continued observation, call `SubscribeTick` with the selected instance and optional `every_seconds` (default 60, range 5–86400). The subscription belongs to the current Loop; retain its returned subscription ID for `UnsubscribeTick`. Report that observation has started only when the result says `ready: true`; `armed` alone means the Trigger is configured. If `ready: false`, use the returned link and diagnostic to inspect the existing subscription instead of creating it again.

Existing content supplies a baseline. Subsequent changes are delivered latest-only at the Loop's cadence, with observed overwrite counts. Quiet periods do not call the model; busy Loops wait under existing execution rules. Stopping or disabling a Loop keeps it stopped.

Protocol Tick Updates carry business content. Read the catalog's recovery query when history matters, respecting its limits. Ordinary Terminal Observations sample recent plain text every 5 seconds, then wait for the Loop cadence. Identical text does not trigger; sampling can miss intermediate output and cannot recover full terminal history. Reads contain at most 200 recent lines / 64 KiB; Loop deliveries may be further truncated to 16 KiB.

Moving a live instance preserves its reference and subscriptions. New operations need its explicit current Workspace scope. A replaced or restarted program has a new identity: retain the old subscription's state and select the new target explicitly. Herdr detach preserves live processes. Closing an Agent Pane closes its View while the daemon-owned Agent continues; closing an ordinary Tick Pane exits that Tick. Stop a Loop explicitly when requested.

Current host state `artifact_ticks` lists artifacts the user added to this Chat. Read and modify their content with the discovered Artifact Service MCP tools. Addition grants no extra permissions and creates neither feedback associations nor Tick Subscriptions.
