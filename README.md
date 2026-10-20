<h1>
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="assets/brand/lockup-on-dark.svg">
    <img src="assets/brand/lockup.svg" alt="Tickloop" width="265" height="50">
  </picture>
</h1>

[Website](https://tickloop.ai) · English | [简体中文](README.zh-CN.md)

Tickloop is a terminal AI agent for code and office work. It understands your project, edits code, runs tests, organizes material, writes documents, builds pages and slide decks, and keeps going based on execution results and feedback. Its distinctive idea is the **Tick**: a live terminal program that both you and the agent use at the same time. The agent can read a Tick, call the tools it declares, and subscribe to it so that changes in the program wake the agent's Loop and drive the work forward.

The main command is `tickloop` (Homebrew also installs the short alias `tl`).

## Features

- **Ticks.** A Tick is one running program, identified as a Tick Instance. *Extension Ticks* speak the [Tick protocol](tick/protocol/SPECIFICATION.en.md) and expose structured state, tools and updates. Any other interactive program (a log viewer, a REPL) can be opened as a *Terminal Tick*, where the agent reads the screen and types input. Background shell commands run as *Shell Ticks*. The Reader (`tick-reader`) ships with Tickloop and reads HTML, Markdown, text and PDF sources.
- **Tickhost.** Every local Tick runs in Tickhost, a host service the agent daemon starts on demand. Ticks keep running when you detach or close a view. The Monitor beside the conversation lists the current Chat's Ticks, subscriptions, artifacts and background shells; press Enter on a row to attach a full view and Esc to hide it again.
- **Herdr panes.** Inside [Herdr](https://github.com/herdrdev/herdr), Tickloop can show a Tick in a split pane next to the agent ("open a K-line Tick to the right of your tab"). A pane is only a view: moving it keeps the instance and its subscriptions, and closing it leaves the Tick running. Ticks work the same outside Herdr; displaying them in tmux or Zellij panes is not supported yet.
- **Subscriptions drive the Loop.** Subscriptions belong to a Loop and are always explicit; opening or reading a Tick never subscribes. Extension and Terminal Ticks are delivered at high (every minute, default), medium (10 minutes) or low (30 minutes) frequency, and only the latest state is delivered. Shell Ticks report their exit immediately.
- **Multiple models.** A built-in catalog covers Anthropic Claude, OpenAI (API and ChatGPT/Codex subscription), GitHub Copilot, Google Gemini and Vertex AI, DeepSeek, Kimi, MiniMax, Zhipu GLM / Z.ai, xAI and Alibaba. Switch models in the middle of a Chat with `/model`; history is carried over to the target protocol.
- **Permissions and modes.** Tool calls go through a permission system with Auto (default, model-reviewed), Ask, Plan and Full Access modes. `shift+tab` cycles modes, and `/permissions` manages rules and shows recent decisions.
- **Subagents.** The `Subagent` tool delegates to real child agents, in the foreground or the background. Members of one family can message each other, nest up to three levels, and optionally share a task list.
- **Artifacts.** The agent can create and publish Docs, Pages and Slides to the Tickloop Artifacts service and receive @tickloop feedback left on them. Sign in with `/artifacts`.
- **Persistent agents.** Agents run in a local daemon. Closing the terminal, a pane or `/exit` only leaves the interface; `/resume` and `tickloop -c` bring a Chat back.

## Requirements

- macOS on Apple silicon or Intel.
- A terminal with true color and a UTF-8 locale. Terminals with the kitty keyboard protocol (Ghostty, kitty, WezTerm, iTerm2 with CSI u) get extra key bindings such as `ctrl+enter`.
- Optional: [Herdr](https://github.com/herdrdev/herdr) to show Ticks in panes.

## Install

`tickloop`, `tickhost` and `tick-reader` are always installed and upgraded together. The daemon only starts a `tickhost` from the same directory and build.

### Homebrew

```sh
brew install tickloopai/tap/tickloop
```

### Install script

```sh
curl -fsSL https://tickloop.ai/install.sh | sh
```

The script installs into `~/.local/bin` and `~/.local/share/tickloop/versions/`, picks the build for your Mac, pins the release tag and verifies SHA-256. It refuses to take over an existing installation from another source. The script's source is [scripts/install.sh](scripts/install.sh).

### Cargo launcher

```sh
cargo install --locked tickloop
```

The `tickloop` crate on crates.io is a small launcher. During `cargo install` it downloads, verifies and embeds the matching official macOS build; see [distribution/cargo](distribution/cargo/README.md).

### From source

Requires the current stable Rust toolchain.

```sh
cargo install --locked --git https://github.com/tickloopai/tickloop \
  tickloop-agent tickloop-tickhost tickloop-tick-reader
```

Add `--tag vX.Y.Z` to pin a release. From a checkout, `make install` installs the same three programs into `~/.cargo/bin`.

### Updating

```sh
tickloop update           # upgrade the whole suite through its installation source
tickloop update --check   # check only
tickloop update --status  # show the last automatic or manual update result
```

Homebrew, install-script and Cargo-launcher installations check for updates in the background without blocking the interface. Updates never stop a running daemon, its agents or its Ticks; new builds take effect the next time you start Tickloop.

## Quick start

1. Run `tickloop` in your project directory. Pass `--cwd PATH` to start somewhere else.
2. Open `/provider` and choose a connection. You are asked for an API key or a sign-in when no credentials exist yet. API keys are stored in `~/.tickloop/.env` with mode `0600`; environment variables such as `DEEPSEEK_API_KEY` also work. The default model is DeepSeek.
3. Describe what you want in plain language. Use `/model` to switch models, `/help` for commands, and `tickloop -c` to continue the most recent Chat in this directory.

To try a protocol Tick without any account, ask the agent:

> Open `python3 tick/examples/counter.py` with `protocol: true` and subscribe to it.

Type `+` and Enter in the Counter's view (Enter on its Monitor row) to produce an update, or ask the agent to call `increment`. The subscribed Loop receives the new state and continues.

State, settings, logins and installed Tick packages live in `~/.tickloop`; set `TICKLOOP_CONFIG_DIR` to use another directory.

## Usage

### Models

- `/provider` lists the built-in connections. Pick one to sign in or enter an API key, then choose a model. Press `p` on a connection for its details, where you can replace or remove saved credentials.
- `/model` lists the models of every configured connection; left/right adjusts Thinking. `/thinking` changes only the Thinking level.
- The model selection and Thinking are saved in `~/.tickloop/settings.json`. Providers and models come from the catalog compiled into the program; external provider definition files are not read.
- Failed model requests show the connection, HTTP status and the service's (redacted) message. To diagnose network failures, start with `TICKLOOP_NETWORK_DIAGNOSTICS=1` and redirect stderr to a file; it logs the failing stage, host, proxy source, status and timing, never URL paths, headers or bodies.

### Commands

| Command | Purpose |
| --- | --- |
| `/help` | Commands grouped by feature (`/help advanced` for the rest) |
| `/resume`, `/clear`, `/branch` | Reopen a saved Chat, start a new one, or branch the current one |
| `/plan` | Read-only planning, then approve the plan before work starts |
| `/permissions`, `/settings` | Execution mode and permission rules; model, Thinking, output style and other settings |
| `/rewind`, `/diff` | Restore code and/or conversation to an earlier prompt; review file changes |
| `/compact`, `/details` | Compact the context; browse and search the detailed transcript (also `ctrl+o`) |
| `/ticks`, `/artifacts` | Manage Tick packages; sign in to and manage Artifacts |
| `/websearch` | Choose the web search service |
| `/exit` | Close this view; the agent keeps running |

On the command line, `tickloop -c` continues the most recent Chat in the directory, `tickloop -r [CHAT]` resumes a Chat (or opens a picker), and `--fork-session` copies the resumed Chat into a new one. `tickloop daemon stop` stops the daemon and every running agent; saved history is kept and Ticks keep running.

### Keys

- `shift+tab` cycles execution modes. `enter` sends; while the agent works, the message is queued and joins at the next tool boundary. `ctrl+enter` interrupts and sends immediately.
- `ctrl+j` (or `alt+enter`/`shift+enter` where the terminal reports them) inserts a newline. `ctrl+g` edits the input in your external editor. Text input has no Vim mode.
- `esc esc` on an empty input opens Rewind. `ctrl+b` sends a foreground Bash command that has run for two seconds to the background.
- Lists and menus use `j`/`k` or `ctrl+n`/`ctrl+p`, `enter` to confirm and `esc` to go back one level.
- In the Monitor, `enter` opens a Tick or Shell view (a popup inside Herdr, full screen otherwise). In a view, `esc` hides it while the program keeps running, and `ctrl+b esc` sends Esc to the program. `s` stops a running Shell, and `x` closes or removes a row after asking.

## Tick packages

Tick packages add data sources. Each package is a prebuilt executable, a manifest (`tick-package.json`) and Skills that teach the agent how to use it. The official packages, currently `longbridge` (quotes, watchlists, K-lines, positions, news, earnings) and `futu` (options through Futu OpenD), are developed and published in [tickloopai/ticks](https://github.com/tickloopai/ticks), which is also the Tick marketplace.

```sh
tickloop tick list                 # installed packages and their login state
tickloop tick list --available     # marketplace packages, versions and platform support
tickloop tick install longbridge
tickloop tick login longbridge     # device-code sign-in; --issuer NAME when there are several
tickloop tick logout longbridge
tickloop tick uninstall longbridge
tickloop tick install --path DIR   # a local package directory or .tar.gz, for package development
```

Inside the agent, `/ticks` offers the same install, update, uninstall, login and logout actions. OAuth logins print a verification link and code that you confirm in a browser on any device, so they also work on remote machines; API-key logins read the key without echoing it. Login states are `logged in`, `needs login`, `login expired`, `no login needed` and `login unknown`.

Installing a package only places its program and Skills: it never runs the program, opens or subscribes to a Tick, or grants market-data, account or trading permission. Credentials are stored in `~/.tickloop/credentials/<package>/` and handed only to that package's own process; `uninstall` keeps them. Installed packages are available to the agent from its next request, and their Skills appear as `<package>:<skill>`, such as `longbridge:usage`.

## Tick protocol

Extension Ticks speak Tick protocol 3: line-delimited JSON-RPC 2.0 over stdin/stdout with `tick/describe`, `tick/read`, `tick/call` and the `tick/update` notification, while the program draws its interface on its controlling terminal. Any program can become a Tick by implementing it.

- [Tick protocol specification](tick/protocol/SPECIFICATION.en.md) ([中文](tick/protocol/SPECIFICATION.md))
- [Counter example](tick/examples/counter.py)
- Rust Tick-side SDK: [`tick/sdk`](tick/sdk)

## Documentation

- [Tick protocol specification](tick/protocol/SPECIFICATION.en.md)
- [Domain glossary](CONTEXT.md) (Chinese)
- [Contributing](CONTRIBUTING.md) and [agent working rules](CLAUDE.md) (Chinese)

## Contributing

Bug reports, ideas and pull requests are welcome. Read [CONTRIBUTING.md](CONTRIBUTING.md) for the build, test and review workflow. For new Tick packages or changes to `longbridge` and `futu`, use [tickloopai/ticks](https://github.com/tickloopai/ticks).

## Security

Do not report vulnerabilities in public issues. See [SECURITY.md](SECURITY.md) for private reporting.

## License

Tickloop is released under the [MIT License](LICENSE). Adapted third-party code and its licenses are listed in [THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md).
