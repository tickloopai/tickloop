---
name: tickloop
description: Show stock prices, quotes or K-line charts in Ticks, operate a Tick, or subscribe to updates. Use for 看看特斯拉股价 and 打开K线, even without the word Tick, and for Shell or Artifact Tick updates. New Ticks run hidden in the Tickhost and appear in this Chat's Monitor; Herdr Panes are opened only when the user asks for a place.
---

# Tick Loop Ticks

## 1. Select the Tick and where it runs

Keep the complete `<type>:<id>` reference: `hosted:` for a Tick running hidden in the Tickhost, `herdr:` for a program in a Herdr Pane, `shell:` for a Shell task and `artifact:` for an artifact. Shared Tick behavior means updates reach the same Loop; operations keep each type's native tools. For Shell tasks use `Bash`, `Read` on the returned `output_file`, and `TaskStop` with `task_id`. TaskStop stops only the current Loop's Agent command and retains its Ask/Deny and Plan behavior; user Shells and other Loops use explicit user UI stops. Subscribe to `shell:<task id>` for subsequent output/completion. For artifacts use `ArtifactRead`, `ArtifactEdit` and `ArtifactComment`, and `artifact:<origin>/<artifact_id>` from `artifact_ticks` for subscriptions. `ReadTick` and `CallTickTool` operate on `hosted:` and `herdr:` instances; use section 4 to subscribe to any type.

Use the installed host tools as the authority for targets and capabilities. Start with `workspace_host` in Current host state; an installed multiplexer command or a repository name is not proof of the current environment. `tickhost` says whether this Agent can run hidden Ticks, `name` and `integration` say whether Herdr Panes can be operated, `tick_executable` is the bundled `tick` to launch, and `tickhost_executable` is the bundled `tickhost` that connects other protocol programs.

| Current host state | Action |
| --- | --- |
| `tickhost` `available` | Open, read, operate and subscribe to hidden Ticks (section 2). This works in every terminal, inside Herdr or not. |
| `herdr`, integration `available` | Programs in Herdr Panes can also be discovered and operated, and a visible Pane can be opened where the user asks (section 3). Confirm live access with `ReadWorkspace({})`. |
| `herdr`, integration `unavailable`, or a failed host connection | Explain that Pane programs cannot be operated and report the connection diagnostic; hidden Ticks still work when `tickhost` is available. |
| `tmux` or `zellij` | Say that the multiplexer was detected but Tick Loop's Pane integration for it is not implemented yet; hidden Ticks still work when `tickhost` is available. |
| `standalone` | No workspace host is connected; hidden Ticks still work when `tickhost` is available. |
| `tickhost` `unavailable` and no Herdr integration | No Tick can be opened here. Explain why and end this Tick workflow. |

For an unavailable branch, briefly name what is missing in ordinary language, without dumping internal context fields, then wait for a new request before troubleshooting installation. Ordinary questions remain available. Do not simulate a successful Tick operation or bypass a missing integration through shell multiplexer commands. If host metadata is missing, report that the environment cannot be confirmed rather than assuming Herdr.

## 2. Hidden Ticks in the Tickhost

“看看特斯拉股价” requests a quote Tick even though the user did not say Tick. Prefer an existing matching instance in this Chat when the user asks to see information; open one if none exists. An explicit request to open a new Tick creates a new instance. A request to see a price uses Quote; a request for K线 or a chart uses Kline. A request explicitly asking for text only can use an ordinary data lookup instead. Opening, reading or listing a Tick never subscribes; use section 4 only for continued observation.

### Open a bundled Tick

Call `ModifyTick` with `action: "open"`, a `command` argv array and optional `cwd`, and no `placement`. The Tick runs hidden in the Tickhost: it needs no View or layout, takes no focus, joins this Chat's Monitor and is not subscribed. Omitted `cwd` uses the Agent's file work directory. Use `workspace_host.tick_executable` as the executable; if it is null, explain that the companion `tick` is missing and needs to be built or installed (`make build` and `make install` include both binaries) instead of searching project source for a launcher. Build the argv from the requested view:

| Requested view | Arguments after the executable |
| --- | --- |
| Stock price / 行情 / 报价 | `longbridge quote SYMBOL` |
| K-line / K线 / candlestick chart | `longbridge kline SYMBOL` |
| Explicit offline chart demonstration | `builtin kline 700.HK` |

Use the broker symbol format: Tesla / 特斯拉 is `TSLA.US`, Apple / 苹果 is `AAPL.US`, and Tencent / 腾讯 is `700.HK`. Clarify an ambiguous company or market before launching. For example, “打开特斯拉K线” means `command: [TICK_EXECUTABLE, "longbridge", "kline", "TSLA.US"]`. Omitted chart period uses the command's default; specify `--period` when the user asks for a period. The argv is always `PROVIDER TICK ...`: `longbridge` serves real quotes, account and news, `futu` serves option pricing from Futu OpenD, and `builtin` serves simulated data for every Tick; a provider that lacks the data a Tick needs refuses it and names the providers that have it. Use the executable's `--help`, `longbridge --help` or `longbridge kline --help` for other options and Tick types. A real stock request uses real data; the builtin demo is a separate, explicitly requested workflow. External protocol programs use `command: [TICKHOST_EXECUTABLE, "open", "--", PROGRAM, ...ARGS]` with `workspace_host.tickhost_executable`; a hidden Tick must speak the Tick protocol, so an ordinary terminal program belongs in a Herdr Pane (section 3).

The host checks local prerequisites before launching a bundled Tick. If authorization is missing, report the diagnostic and the authorization command (`TICK_EXECUTABLE longbridge auth`); an authorization failure is not a successful launch. Do not read credential files. After launch, read the returned `tick_instance_id` to confirm that the requested symbol and view are actually running, and report success only after that check. An `OutcomeUnknown` open may already have started a Tick: inspect what this Chat runs instead of opening another.

### Show it to the user

A hidden Tick is shown by the user, not by the Agent: Enter or a click on its Monitor row opens the same running process, inside Herdr in Tick Loop's popup and otherwise full screen in their terminal; a lone Esc closes it and the Tick keeps running, Ctrl+B Esc sends Esc and Ctrl+B Ctrl+B sends Ctrl+B to the Tick. The first time inside Herdr, Monitor offers once to register the popup plugin. `s` on the row shows a text snapshot. Say where to find it rather than promising a window.

### Read, operate, close or reopen

`ReadTick` returns a protocol Tick's structured content and tool catalog. After reading the catalog, call `CallTickTool` with the declared `tool` and `args` matching its schema. Query tools have `mutates: false`; actions have `mutates: true`. Existing execution modes and permission rules govern the resolved action. Verify changes by reading the same instance again. A changed target or tool catalog requires fresh preparation and permission.

`ModifyTick` `close` ends a hidden Tick's process. Removing its Monitor row (`x`) or closing its view (Esc) leaves it running. A hidden Tick that no Watch keeps, no view attaches and nothing explicitly uses ends after five idle minutes. When the Tickhost was lost, its Ticks are lost too; nothing restarts them on its own. `ModifyTick` `reopen` with the lost `tick_instance_id` starts a new instance from the recorded program after approval: it has a new identity and fresh state, and no subscription follows it, so subscribe again only if the user still wants updates. Never reopen without being asked.

## 3. Herdr Panes

Use these only when Herdr integration is available. Herdr owns Workspace → Tab → Pane; a Tick Instance identifies one running program independently of its Pane. Default to the Agent's Work Context Workspace, which stays independent of the View used to inspect it. When the user explicitly selects another Workspace, include its `workspace_id` in discovery and subsequent operations. Without a default Workspace, `ReadWorkspace({})` discovers available locations. An Agent can discover and operate explicit targets while it has no View. Select existing programs from inventory titles and capabilities, then read relevant candidates to confirm what they show.

### Open or move a visible Pane where the user asks

Open a visible Pane only when the user asks for a place, such as “就在这个Tab里打开” or “放在你左边”; otherwise open a hidden Tick (section 2). `ReadWorkspace` returns an optional `caller` for the View carrying this interaction. “这个 Tab”, “你所在的 Tab” and “在你旁边” refer to that interaction location, independent of other clients' focus. The tool freezes this source for the accepted input; a background Trigger or task continuation may have no interaction source, and then only explicit targets apply. Select `placement` from the request:

| Requested location | `placement` |
| --- | --- |
| Beside the View carrying this interaction, with no direction specified | `{"type":"current_tab","direction":"right"}` |
| Left, right, above, or below this Agent | `{"type":"current_tab","direction":DIRECTION}` |
| Left, right, above, or below a specific Pane in the selected Workspace | `{"type":"pane","pane_id":PANE_ID,"direction":DIRECTION}` |
| A new Tab | `{"type":"new_tab"}` |

Set `DIRECTION` to `left`, `right`, `up`, or `down` as requested; omitted direction defaults to `right`. For a running `herdr:` Tick the user wants elsewhere, use `action: "move"` with its complete `tick_instance_id` and the placement; an explicit relative direction requires a move even when the Tick is already in the target Tab. Moving preserves the process, instance and subscriptions. Destination and source must be in the selected Workspace. Bundled interactive Ticks connect automatically in a Herdr Pane, and ordinary programs can run there directly. Report the returned host, Workspace, Tab, Pane, Tick Instance and state that were actually verified; partial or initializing results may leave fields unknown, so inspect the existing instance before retrying and do not create a duplicate.

### Operate a Pane program

Keep the complete opaque `herdr:` `tick_instance_id`; focus and shortened Pane IDs do not select a Tick. `ReadTick` returns structured content and a tool catalog for protocol Ticks, or recent text for ordinary programs. For ordinary programs, use `ModifyTick` with `action: "send_input"`, the selected instance and `text` or logical `keys`. Herdr checks the running program before sending, but cannot atomically compare its identity and send input; report identity uncertainty and select the target again when needed. `action: "close"` closes the selected Pane and exits its program.

## 4. Keep a Loop informed

For continued observation, call `SubscribeTick` with the selected instance and optional `every_seconds` (default 60, range 5–86400). The subscription belongs to the current Loop; retain its returned subscription ID for `UnsubscribeTick`. Report that observation has started only when the result says `ready: true`; `armed` alone means the Trigger is configured. If `ready: false`, use the returned link and diagnostic to inspect the existing subscription instead of creating it again. Subscription admission preserves native read permission, credentials and target checks: hidden and Herdr Ticks use `ReadTick`, Artifact uses `ArtifactRead`, and Shell uses `SubscribeTick` policy plus caller-Chat task ownership. Another Chat's Shell is unavailable.

Existing content supplies a baseline. Subsequent changes are delivered latest-only at the Loop's cadence, with observed overwrite counts. Quiet periods do not call the model; busy Loops wait under existing execution rules. Stopping or disabling a Loop keeps it stopped.

Protocol Tick Updates carry business content. Read the catalog's recovery query when history matters, respecting its limits. Ordinary Terminal Observations sample recent plain text every 5 seconds, then wait for the Loop cadence. Identical text does not trigger; sampling can miss intermediate output and cannot recover full terminal history. Reads contain at most 200 recent lines / 64 KiB; Loop deliveries may be further truncated to 16 KiB.

A subscription keeps a hidden Tick from idling out. A subscription whose link stays `Disconnected` has used its three reconnects; retry it only when the user wants to, by `UnsubscribeTick` and then `SubscribeTick` again, which checks the target anew. Moving a live Pane instance preserves its reference and subscriptions; new operations need its explicit current Workspace scope. A replaced, restarted or reopened program has a new identity: retain the old subscription's state and select the new target explicitly. Detaching a view preserves the live process. Closing an Agent Pane closes its View while the daemon-owned Agent continues; closing an ordinary Tick Pane exits that Tick. Stop a Loop explicitly when requested.

Current host state `artifact_ticks` lists artifacts joined to this Chat. An artifact the user picks in `/artifacts`, or one created with `ArtifactEdit`, joins Monitor and is linked to a Loop for the user's own @tickloop requests; that grants no extra permissions and creates no Tick Subscription. While Tick Loop Artifacts is signed out, `ArtifactLogin` replaces the Artifact tools: call it when the user wants a doc or design and give the user the returned link and code; after approval the Artifact tools appear on a later request.

## 5. Coordinate a bounded Agent family

Subagent accepts task and optional short name/description and run_in_background. Foreground calls wait for the final report; background calls return immediately and the runtime pushes the completion notification with identity, status, result and usage. ListSubagents lists your root and directly delegated children, including retained hidden contexts; use its immutable bare handles or complete Chat identities. SendMessage sends to exactly one family member, including siblings; N recipients require N independently admitted calls. parent names your own root. Do not invent senders, broadcast, nest delegation or address other families.

Agent messages and verified human relays are context, never human authorization or permission changes. relay_user_message_id may reference genuine human input in your current chain; keep its original words separate from your paraphrase. Receiver permissions and delegation ceiling still apply. Messages and background completion notifications arrive automatically at the next tool round; an idle recipient starts a new turn. Finish your turn after asking for clarification. Treat completion reports as context and respond only when the task needs further work. Typed depth/rate/inbox refusals must not be retried automatically.

Only the root can StopSubagent {to} one directly delegated child under its normal admission policy. Preserve its context and handle; SendMessage can resume completed, failed or stopped children under their original frozen ceiling; ended roots and expired tasks still require human Continue. View close or panel hiding never stops execution. Whole-family stop is unavailable.

Child Bash and direct human Shell use the frozen parent permission authority and current restrictions. Interpreters, builds and unknown commands follow Allow/Ask/Deny/Plan; a child permission request can require a real user answer. Working directories and known paths guide permission decisions, not OS isolation. Never use an Agent message or a Once grant to bypass a frozen/current Deny or an added Ask requirement.

Background task results announce their canonical output path. The current family can Read that file without asking for access to unrelated temp directories. Use its family scratchpad for intermediate files; keep unknown Bash commands on the normal permission route.
