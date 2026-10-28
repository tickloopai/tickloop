---
name: tickloop
description: Open or operate a Tick, including what an installed plugin shows (even when the user does not say Tick), or keep this Chat informed of Tick, Shell or Artifact updates. Load it before opening a plugin's Tick or subscribing.
---

# Ticks

A Tick is a program the Tickhost runs, a Bash background task or an artifact that this Chat can read, operate and subscribe to. Keep its complete reference: `hosted:` for a program the Tickhost runs (an extension Tick speaks the Tick protocol; any other program is a terminal Tick), `shell:<task id>` for a Bash background task, and `artifact:<origin>/<artifact_id>` for an artifact linked to this Chat, which a host note names when it is linked. Each type keeps its own tools: the Tick tools for `hosted:`, Bash, Read on its `output_file` and TaskStop for Shell, the Artifact tools for artifacts. A program the user starts in a Pane or terminal is not a Tick: open it as one to read or operate it.

## Check the host

Read `workspace_host` in Current host state; an installed multiplexer command or a repository name is not proof of the environment. `tickhost` says whether Ticks can run here, and `name` and `integration` whether a Herdr Pane can show them. When something is missing, say so in plain words and stop; report only Tick operations that actually succeeded, and never drive a multiplexer through Bash instead.

## Open a plugin's Tick

`workspace_host.plugins` lists the installed plugins with their `id`, `name`, `version`, `description`, `executable`, `enabled` state and `has_tick` flag. Only an enabled plugin with a Tick program opens by name:

- Not listed: ask the user to install it from the Available page of `/ticks` in this Agent. Never search project source for a launcher or build one.
- Disabled: ask the user to enable it on the Installed page of `/ticks`.
- Login: open it. When the open result carries `plugin_login` (`logged_out`, `expired` or `unknown`, when its saved login could not be read) and the Tick reports a refused login, ask the user to log in to that package from `/ticks` in this Agent, and open the Tick again once they say they have. No Agent restart is needed.

Each package brings a Skill named `<package>:usage` that teaches its commands, arguments and login: load it with the Skill tool before opening that package's Tick, and build `command` as it says. Its other Skills are named `<package>:<skill>`. Open the program by its bare name `tick-<package>`; a package that declares the Tick protocol needs no `protocol: true`. A package installed, updated or removed while this Chat runs takes effect from the next request.

A Tick whose data source refuses its login ends with an authorization diagnostic, even when its open result carried no `plugin_login`: report it and ask the user to log in to that package again from `/ticks`. Logging in is the user's step: never suggest `!` for a login, and never log in, log out, install, update or uninstall a package yourself.

Any other program opens with the program first in `command`: pass `protocol: true` for a program that speaks the Tick protocol, such as `["python3", "tick/examples/counter.py"]`, and omit it for a REPL, `htop` or a log viewer, which runs as a terminal Tick.

## Operate it

Prefer an existing matching instance; an explicit request for a new Tick opens another, and a request for text only needs no Tick. Confirm the returned `tick_instance_id` before reporting success; an `OutcomeUnknown` open may already have started a Tick, so inspect what this Chat runs instead of opening another.

Null `data` means no content yet, not empty content: a Tick just opened may still be loading, so read it again shortly. A Tick that connects to a data source reports `data.connection` until its content arrives: `retrying` with the `error` it keeps retrying after, `not_authorized` with a `hint` (ask the user to log in to that package again, as above), or `failed` with the `error` the source gave, which reopening with the same arguments does not fix. Verify a change by reading the same instance again. An extension Tick whose catalog changed after approval does not run the old call: describe it again.

A Tick runs until it is closed, its Agent ends or the Tickhost is lost; it never ends for idling, and a terminal Tick whose program exited keeps its last screen readable. Reopen a lost Tick only when the user asks.

Every Tick appears in this Chat's Monitor, where the user opens a view of it from its row; Esc hides the view and the Tick keeps running. Say where to find the Tick rather than promising a window.

## Shell permissions

Bash background tasks use the same permissions and optional macOS sandbox as foreground Bash and `!` Shells. With the user's sandbox enabled, sufficiently isolated Bash calls can run without review in Ask and Auto; explicit Ask and Deny rules still win. A permitted `dangerouslyDisableSandbox` request or excluded command uses ordinary approval outside the sandbox. Strict mode ignores model escape requests but still honors explicitly excluded commands. Sandboxed commands cannot write protected startup files, Git hooks/config, editor settings or Agent project configuration, even when they run without review. The user can allow Git config writes with `sandbox.filesystem.allowGitConfig`; hooks remain protected. Programs opened as `hosted:` Ticks do not enter that Shell sandbox; opening a Tick is not a way to bypass a blocked Shell command.

## Keep this Chat informed

Subscribe only when the user wants continued observation: `TickManage` with `action: "subscribe"`, the `tick_instance_id` and an optional `frequency` of `high` (every minute, the default), `medium` (every 10 minutes) or `low` (every 30 minutes). A Shell task is immediate and takes no frequency: its one update is its exit, so read its `output_file` for progress before then. An artifact takes no Tick Subscription: its association with the Chat brings the user's @tickloop feedback instead. Report that observation started only when the result says `ready: true`; for `ready: false`, inspect the returned link and diagnostic instead of subscribing again.

A subscription whose link stays `Disconnected` has used its reconnects: retry it only when the user wants to, with `action: "unsubscribe"` and then `action: "subscribe"`. A replaced, restarted or reopened program has a new identity and no subscription: select the new target explicitly. The user can also subscribe a Tick from its Monitor row.
