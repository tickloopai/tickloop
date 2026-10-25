<h1>
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="assets/brand/lockup-on-dark.svg">
    <img src="assets/brand/lockup.svg" alt="Tickloop" width="265" height="50">
  </picture>
</h1>

[Website](https://tickloop.ai) · English | [简体中文](README.zh-CN.md)

Tickloop is a terminal AI agent for code and office work. It understands your project, edits code, runs tests, organizes material, writes documents, builds pages and slide decks, and keeps going based on execution results and feedback. Its distinctive idea is the **Tick**: a live terminal program that both you and the agent use at the same time. The agent can read a Tick, call the tools it declares, and subscribe to it so that changes in the program wake the agent's Loop and drive the work forward.

The main command is `tickloop`. Homebrew, the install script and the Cargo launcher also install the short alias `tl`.

## Features

- **Ticks.** A Tick is one running program, identified as a Tick Instance. *Extension Ticks* speak the [Tick protocol](tick/protocol/SPECIFICATION.en.md) and expose structured state, tools and updates. Any other interactive program (a log viewer, a REPL) can be opened as a *Terminal Tick*, where the agent reads the screen and types input. Background shell commands run as *Shell Ticks*.
- **Tickhost.** Every local Tick runs in Tickhost, a host service the agent daemon starts on demand. Ticks keep running when you detach or close a view. The Monitor beside the conversation lists the current Chat's Ticks, subscriptions, artifacts, background shells and scheduled tasks; press Enter on a row to attach a full view and Esc to hide it again.
- **Herdr panes.** Inside [Herdr](https://github.com/herdrdev/herdr), Tickloop can show a Tick in a split pane next to the agent ("open the counter Tick to the right of your tab"). A pane is only a view: moving it keeps the instance and its subscriptions, and closing it leaves the Tick running. Ticks work the same outside Herdr; displaying them in tmux or Zellij panes is not supported yet.
- **Subscriptions drive the Loop.** Subscriptions belong to a Loop and are always explicit; opening or reading a Tick never subscribes. Extension and Terminal Ticks are delivered at high (every minute, default), medium (10 minutes) or low (30 minutes) frequency, and only the latest state is delivered. Shell Ticks report their exit immediately.
- **Multiple models.** A built-in catalog covers Anthropic Claude, OpenAI (API and ChatGPT/Codex subscription), GitHub Copilot, Google Gemini and Vertex AI, DeepSeek, Kimi, MiniMax, Zhipu GLM / Z.ai, xAI and Alibaba. Switch models in the middle of a Chat with `/model`; history is carried over to the target protocol.
- **Permissions and modes.** Tool calls go through a permission system with Auto (default, model-reviewed), Ask, Plan and Full Access modes. `shift+tab` cycles the modes ticked in `/settings` → `shift+tab modes` (Auto and Plan by default). `/permissions` chooses the execution mode, configures Auto review and shows recent decisions; the advanced commands `/rules` and `/rule` list permission rules and add or remove one.
- **Subagents.** The `Subagent` tool delegates to real child agents, in the foreground or the background. Members of one family can message each other, nest up to three levels, and optionally share a task list.
- **Artifacts.** The agent can create and publish Docs, Pages and Slides to the Tickloop Artifacts service and receive @tickloop feedback left on them. Sign in with `/artifacts`.
- **Persistent agents.** Agents run in a local daemon. Closing the terminal, a pane or `/exit` only leaves the interface; `/resume` and `tickloop -c` bring a Chat back.

## Scheduled tasks

`/loop 5m check the deploy` runs an instruction on a fixed interval (once right away, then on schedule), and `/loop check the deploy` lets the agent choose the next wakeup at the end of each turn and say why. The interval can come first (`30m`) or last (`every 2 hours`). Seconds round up to whole minutes, and an interval that no cron schedule repeats evenly uses the nearest one that does and tells you the actual value.

Without an instruction, `/loop` and `/loop 5m` reread the project's `.tickloop/loop.md` at every run, then the configuration root's `loop.md` (up to 25,000 bytes); when neither exists, the agent continues unfinished work, looks after the current branch's pull request and cleans up code this work touched. A Chat without a Folder reads only the configuration root's `loop.md`. Maintenance instructions do not authorize new work or irreversible actions you have not allowed.

The bottom of the Monitor shows each scheduled task in two lines (`◷ cadence · next run` and the instruction; up to three tasks, the rest under `+N more`), with progress during the 30 minutes before the next run. `enter` shows the cron, the full instruction, the creation and expiry times and skipped runs; pressing `ctrl+x` twice deletes the task and `p` pauses or resumes it. You can also ask the agent to list, change or delete tasks, or to set a one-time reminder. Recurring tasks run one last time after 7 days and are then deleted.

`esc` and `/stop` end a self-paced `/loop` and delete its task; fixed-interval and one-time tasks keep running. `/stop` also pauses receiving until you send a new message or Continue. After you leave the interface, scheduled tasks keep running in the local daemon.

## Requirements

- macOS on Apple silicon or Intel, or Linux on ARM64 or x86_64 (static builds that run on glibc and musl distributions).
- A terminal with true color and a UTF-8 locale. Terminals with the kitty keyboard protocol (Ghostty, kitty, WezTerm, iTerm2 with CSI u) get extra key bindings such as `ctrl+enter`.
- Optional: [Herdr](https://github.com/herdrdev/herdr) to show Ticks in panes.

## Install

`tickloop` and `tickhost` are always installed and upgraded together. The daemon starts the `tickhost` installed beside it (falling back to the one on PATH) and keeps using a running Tickhost while its protocol stays compatible.

### Homebrew

```sh
brew install tickloopai/tap/tickloop
```

### Install script

```sh
curl -fsSL https://tickloop.ai/install.sh | sh
```

The script installs into `~/.local/bin` and `~/.local/share/tickloop/versions/`, picks the build for your platform, pins the release tag and verifies SHA-256. When `~/.local/bin` is not on PATH, it adds it once to your login shell's configuration (`~/.zshrc` for zsh; `~/.bashrc` plus `~/.bash_profile` or `~/.profile` for bash; fish's `conf.d`; otherwise `~/.profile`) and names the files it changed. It refuses to take over an existing installation from another source. The script's source is [scripts/install.sh](scripts/install.sh).

### Cargo launcher

```sh
cargo install --locked tickloop
```

The `tickloop` crate on crates.io is a small launcher. During `cargo install` it downloads, verifies and embeds the matching official macOS or Linux build; see [distribution/cargo](distribution/cargo/README.md).

### From source

Requires the current stable Rust toolchain.

```sh
cargo install --locked --git https://github.com/tickloopai/tickloop \
  tickloop-agent tickloop-tickhost
```

Add `--tag vX.Y.Z` to pin a release. From a checkout, `make install` installs the same two programs into `~/.cargo/bin`.

### Updating

```sh
tickloop update           # upgrade the whole suite through its installation source
tickloop update --check   # check only
tickloop update --status  # show the last automatic or manual update result
```

Homebrew, install-script and Cargo-launcher installations check for updates in the background without blocking the interface. Updates never stop a running daemon, its agents or its Ticks. Compatible processes switch to the new build automatically at safe points (an agent after its current turn); no restart is needed.

## Quick start

1. Run `tickloop` in your project directory. Pass `--cwd PATH` to start somewhere else.
2. The first launch opens model selection: pick a connection, enter an API key or sign in when no credentials exist yet, then choose a model. Messages are not sent until a model is chosen. Use `/provider` to change it later. API keys are stored in `~/.tickloop/.env` with mode `0600`; environment variables such as `DEEPSEEK_API_KEY` also work.
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
| `/goal`, `/loop` | Set a completion condition the agent keeps working toward; repeat an instruction on an interval or at a self-paced cadence |
| `/stop`, `/monitor` | Interrupt the turn, pause Triggers until your next message and stop subagents; show or hide the Monitor |
| `/permissions`, `/settings` | Execution mode, Auto review and recent permission decisions; model, Thinking, output style and other settings |
| `/rewind`, `/diff` | Restore code and/or conversation to an earlier prompt; review file changes |
| `/compact`, `/details` | Compact the context; browse and search the detailed transcript (also `ctrl+o`) |
| `/ticks`, `/artifacts` | Manage Tick packages; sign in to and manage Artifacts |
| `/websearch` | Choose the web search service |
| `/exit` | Close this view; the agent keeps running |

On the command line, `tickloop -c` continues the most recent Chat in the directory, `tickloop -r [CHAT]` resumes a Chat (or opens a picker), and `--fork-session` copies the resumed Chat into a new one. `tickloop daemon stop` stops the daemon and every running agent; saved history is kept, and extension and Terminal Ticks keep running in Tickhost, while background Shells end with their agents.

### Keys

- `shift+tab` cycles the execution modes ticked in `/settings` (Auto and Plan by default). `enter` sends; while the agent works, the message is queued and joins at the next tool boundary. `ctrl+enter` interrupts and sends immediately.
- `ctrl+j` (or `alt+enter`/`shift+enter` where the terminal reports them) inserts a newline. `ctrl+g` edits the input in your external editor. Text input has no Vim mode.
- When idle, `esc esc` clears the draft, or opens Rewind on an empty input. While the agent works (or a Goal or self-paced `/loop` is active), `esc` interrupts the turn, pauses the Goal and ends a self-paced `/loop`. `ctrl+b` sends a foreground Bash command that has run for two seconds to the background.
- Lists and menus use `j`/`k` or `ctrl+n`/`ctrl+p`, `enter` to confirm and `esc` to go back one level.
- In the Monitor, `enter` opens a Tick or Shell view (a popup inside Herdr, full screen otherwise). In a view, `esc` hides it while the program keeps running, and `ctrl+b esc` sends Esc to the program. Press `ctrl+x` twice within three seconds on a row to close a Tick, stop a Shell, remove a row, unlink an artifact or delete a scheduled task; `s` cycles a Tick's subscription frequency, `p` pauses or resumes a subscription or scheduled task, and `enter` on a Tick that ended reopens it with its subscriptions.

## Tick packages

Tick packages add data sources. Each package is a prebuilt executable, a manifest (`tick-package.json`) and Skills that teach the agent how to use it. The official packages are developed and published in [tickloopai/ticks](https://github.com/tickloopai/ticks), which is also the Tick marketplace.

```sh
tickloop tick list                 # installed packages and their login state
tickloop tick list --available     # marketplace packages, versions and platform support
tickloop tick install PACKAGE
tickloop tick login PACKAGE        # device-code sign-in; --issuer NAME when there are several
tickloop tick logout PACKAGE
tickloop tick uninstall PACKAGE
tickloop tick install --path DIR   # a local package directory or .tar.gz, for package development
```

Inside the agent, `/ticks` offers the same install, update, uninstall, login and logout actions. OAuth logins print a verification link and code that you confirm in a browser on any device, so they also work on remote machines; API-key logins read the key without echoing it. Login states are `logged in`, `needs login`, `login expired`, `no login needed` and `login unknown`.

Installing a package only places its program and Skills: it never runs the program, opens or subscribes to a Tick, or grants any data, account or other permission. Credentials are stored in `~/.tickloop/credentials/<package>/` and handed only to that package's own process; `uninstall` keeps them. Installed packages are available to the agent from its next request, and their Skills appear as `<package>:<skill>`, such as `<package>:usage`.

## Tick protocol

Any program can become an extension Tick by speaking the Tick protocol; see the [specification](tick/protocol/SPECIFICATION.en.md) ([中文](tick/protocol/SPECIFICATION.md)), the [counter example](tick/examples/counter.py) and the Rust Tick-side SDK [`tick/sdk`](tick/sdk).

## Contributing

Bug reports, ideas and pull requests are welcome. Read [CONTRIBUTING.md](CONTRIBUTING.md) for the build, test and review workflow; domain terms are in [CONTEXT.md](CONTEXT.md) and the working rules for coding agents in [AGENTS.md](AGENTS.md) (both Chinese). For new Tick packages or changes to the official ones, use [tickloopai/ticks](https://github.com/tickloopai/ticks).

## Security

Do not report vulnerabilities in public issues. See [SECURITY.md](SECURITY.md) for private reporting.

## License

Tickloop is released under the [MIT License](LICENSE). Adapted third-party code and its licenses are listed in [THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md).
