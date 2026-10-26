# Tickloop

[Website](https://tickloop.ai) · English | [简体中文](README.zh-CN.md)

Tickloop is an AI agent that works in your terminal. Tell it what you need in plain language: it reads your project, edits code, runs tests, organizes material, writes documents and builds pages or slide decks, and keeps going until the job is done.

What makes it different are **Ticks**: live programs, such as a log viewer, a dashboard or a long-running command, that you and the agent watch together. The agent can read a Tick, use it, and be woken up when something in it changes.

## What you can do

- **Hand off real work.** The agent edits files, runs commands and checks the results. By default it works in Auto mode, where a reviewer model approves routine actions and asks you about risky ones.
- **Keep an eye on things.** Open a program as a Tick and subscribe to it; the agent reacts when it changes. The Monitor beside the conversation lists your Ticks, background commands and scheduled tasks.
- **Repeat work on a schedule.** `/loop 5m check the deploy` runs an instruction every five minutes; `/goal` sets a condition the agent keeps working toward.
- **Use the model you like.** Claude, OpenAI (API or ChatGPT subscription), GitHub Copilot, Gemini, DeepSeek, Kimi, MiniMax, GLM, xAI and Alibaba are built in. Switch at any time with `/model`.
- **Share results.** Publish docs, pages and slides to Tickloop Artifacts and get feedback from others.
- **Leave and come back.** Closing the terminal does not stop the agent. `tickloop -c` picks up the latest conversation.

## Install

macOS:

```sh
brew install tickloopai/tap/tickloop
```

macOS or Linux:

```sh
curl -fsSL https://tickloop.ai/install.sh | sh
```

With Rust installed, `cargo install --locked tickloop` also works. All three install the `tickloop` command and its short alias `tl`, and keep themselves up to date in the background. Run `tickloop update` to update right away.

## Get started

1. Run `tickloop` in your project folder.
2. On first launch, pick a model provider, enter its API key or sign in, then choose a model.
3. Describe what you want done.

Want to see a Tick in action? Ask the agent:

> Open `python3 tick/examples/counter.py` with `protocol: true` and subscribe to it.

Then press Enter on the Counter in the Monitor, type `+` and press Enter: the agent receives the new count and responds.

## Everyday use

| Command | What it does |
| --- | --- |
| `/help` | List all commands |
| `/model`, `/provider` | Switch models, or sign in to another provider |
| `/plan` | Plan first, then approve before anything changes |
| `/goal`, `/loop` | Keep working toward a goal; repeat an instruction |
| `/stop` | Stop everything the agent is doing |
| `/rewind`, `/diff` | Undo back to an earlier message; review file changes |
| `/resume`, `/clear` | Reopen a past conversation; start a new one |
| `/permissions`, `/settings` | Choose how much the agent may do on its own; other preferences |
| `/plugins`, `/artifacts` | Manage plugins; sign in to Artifacts |
| `/exit` | Leave the interface (the agent keeps running) |

Keys:

- `enter` sends a message. While the agent works it waits for the next step; `ctrl+enter` interrupts and sends right away.
- `esc` interrupts the agent. Press it twice on an empty input to undo to an earlier message.
- `shift+tab` switches between Auto and Plan mode.
- `ctrl+j` inserts a new line; `ctrl+g` edits your message in an external editor.
- In lists and menus, use `j`/`k` or the arrow keys, `enter` to choose and `esc` to go back.
- In the Monitor, `enter` opens a Tick and `esc` hides it again; the program keeps running.
- To close/remove a Tick, stop a Shell, unlink an Artifact or delete a scheduled task in Monitor, press plain `x` twice on the same row within three seconds. The first press shows a red `Press x again to …` hint, with no dialog. Expiry, selection movement, another key or leaving cancels it. `Ctrl+X` does not perform these actions; in text inputs and filters, `x` stays text.

## Plugins and data sources

Plugins add Skills and Tick programs. Open `/plugins` to browse installed and available plugins, or use the command line:

```sh
tickloop plugin list --available
tickloop plugin install NAME@MARKETPLACE
tickloop plugin list
tickloop plugin login NAME                  # when the plugin requires login
tickloop plugin logout NAME
tickloop plugin update NAME                 # omit NAME to update all marketplace plugins
tickloop plugin disable NAME
tickloop plugin enable NAME
tickloop plugin uninstall NAME              # deletes plugin data; keeps saved login
tickloop plugin uninstall NAME --keep-data  # keeps plugin data too
```

The built-in marketplaces are `tickloop` and `claude-plugins-official`. They cannot be removed; the Git marketplace downloads on first use. Official Tick packages live in [tickloopai/ticks](https://github.com/tickloopai/ticks). Install with just `NAME` when its source is unambiguous; if several marketplaces offer it, the command lists candidates (or uses the source of an existing installation). To switch an installed name to another marketplace, uninstall it first.

Register other sources from the command line:

```sh
tickloop plugin marketplace add OWNER/REPO
tickloop plugin marketplace list
tickloop plugin marketplace update MARKETPLACE  # omit the name to refresh all
tickloop plugin marketplace remove MARKETPLACE
```

`add` also accepts `OWNER/REPO#REF`, Git URLs (HTTPS, SSH or file), a local directory, or an HTTPS marketplace JSON URL. It uses the name declared by the marketplace. Removing a marketplace keeps its installed plugins. Browsing uses the last fetched Git catalog; refresh it with `marketplace update`. The `tickloop` catalog is read on every use. Plugin `update` refreshes its source and checks Git commits, so it can update content even when the displayed version stays the same. Update-all skips plugins from removed marketplaces.

For a local plugin directory or `.tar.gz` archive:

```sh
tickloop plugin validate ./my-plugin
tickloop plugin install --path ./my-plugin
```

Local-path installations are updated only by running `install --path` again, even at the same version; `update` and the panel do not update them. `validate` reports the manifest, components and path warnings; installation additionally checks the file tree. A failed installation leaves the previous installation intact. If a plugin is marked unusable, reinstall it from its original source.

In `/plugins`, `Tab` or left/right switches between **Installed** and **Available**. Use `j`/`k` or `Ctrl+N`/`Ctrl+P` to select and `Enter` to install or open actions. Installed rows show source, version, enabled state, login and unsupported components; actions include enable/disable, update and login/logout. On Available, `/` starts a case-insensitive filter across names, descriptions and marketplaces: `j`/`k` enter text, `Ctrl+N`/`Ctrl+P` select, and `Ctrl+G` opens an editor. `Esc` leaves filtering and keeps the query; another `Esc` closes the panel. A failed marketplace has its own error row, summarized in at most three lines; other sources and Installed remain usable.

To uninstall in the panel, press `x` twice on the same Installed row within three seconds. This deletes plugin data, keeps saved login, and leaves running Ticks running. Moving selection, another key, leaving the panel or waiting cancels confirmation. Use the CLI's `--keep-data` to preserve data. Saved logins remain under `credentials/NAME/` in your configuration directory; use `logout` before uninstalling if you want to remove the saved login.

Type `/` to discover user-invocable Skills for the current folder, including built-in, user, project and enabled plugin Skills. The Skills group shows each name, description and source; use `Ctrl+N` / `Ctrl+P` to select, then `Enter` or `Tab` to complete. Completion refreshes when Skills or plugins change. Invoke `/plugin:skill [ARGUMENTS]`, or `/skill [ARGUMENTS]` when that short name is unique. Ambiguous short names list the full names to use; exact names win, and built-in commands always take precedence. Each invocation displays a `Skill · name · source` row, preserved when reopening history.

Enabled plugin Skills appear as `plugin:skill` from the next model request, without restarting. Disabling hides its Skills and prevents opening its Tick by name; reinstalling or updating preserves the disabled state. Installation runs nothing and grants no permissions. Tick programs run as you only when opened under your permission settings; plugin Skill tool calls still require the usual approval. Only Skills and Tick components work in this release; other components are listed as unsupported. npm and pip plugin sources are listed but cannot be installed. Installation, updates, removal and login are user actions, not actions the model initiates.

Existing Tick packages migrate automatically on first use, keeping saved login. Broken packages are reported for reinstallation; the old directory is preserved. `tickloop tick …` and `/ticks` remain aliases in this release and show a rename hint.

Any program can become an Extension Tick by speaking the [Tick protocol](tick/protocol/SPECIFICATION.md); start from the [counter example](tick/examples/counter.py).

## Requirements

- macOS (Apple silicon or Intel) or Linux (ARM64 or x86_64).
- A terminal with true color and UTF-8. Ghostty, kitty, WezTerm or iTerm2 enable extra keys such as `ctrl+enter`.

Settings, logins and history are kept in `~/.tickloop`.

## Contributing

Bug reports, ideas and pull requests are welcome; see [CONTRIBUTING.md](CONTRIBUTING.md). To build from source, see its development setup.

## Security

Please report vulnerabilities privately as described in [SECURITY.md](SECURITY.md), not in public issues.

## License

[MIT](LICENSE). Third-party code and licenses are listed in [THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md).
