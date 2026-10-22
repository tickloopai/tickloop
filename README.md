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

- **Ticks.** A Tick is one running program, identified as a Tick Instance. *Extension Ticks* speak the [Tick protocol](tick/protocol/SPECIFICATION.en.md) and expose structured state, tools and updates. Any other interactive program (a log viewer, a REPL) can be opened as a *Terminal Tick*, where the agent reads the screen and types input. Background shell commands run as *Shell Ticks*.
- **Tickhost.** Every local Tick runs in Tickhost, a host service the agent daemon starts on demand. Ticks keep running when you detach or close a view. The Monitor beside the conversation lists the current Chat's Ticks, subscriptions, artifacts and background shells; press Enter on a row to attach a full view and Esc to hide it again.
- **Herdr panes.** Inside [Herdr](https://github.com/herdrdev/herdr), Tickloop can show a Tick in a split pane next to the agent ("open the counter Tick to the right of your tab"). A pane is only a view: moving it keeps the instance and its subscriptions, and closing it leaves the Tick running. Ticks work the same outside Herdr; displaying them in tmux or Zellij panes is not supported yet.
- **Subscriptions drive the Loop.** Subscriptions belong to a Loop and are always explicit; opening or reading a Tick never subscribes. Extension and Terminal Ticks are delivered at high (every minute, default), medium (10 minutes) or low (30 minutes) frequency, and only the latest state is delivered. Shell Ticks report their exit immediately.
- **Multiple models.** A built-in catalog covers Anthropic Claude, OpenAI (API and ChatGPT/Codex subscription), GitHub Copilot, Google Gemini and Vertex AI, DeepSeek, Kimi, MiniMax, Zhipu GLM / Z.ai, xAI and Alibaba. Switch models in the middle of a Chat with `/model`; history is carried over to the target protocol.
- **Permissions and modes.** Tool calls go through a permission system with Auto (default, model-reviewed), Ask, Plan and Full Access modes. `shift+tab` cycles modes, and `/permissions` manages rules and shows recent decisions.
- **Subagents.** The `Subagent` tool delegates to real child agents, in the foreground or the background. Members of one family can message each other, nest up to three levels, and optionally share a task list.
- **Artifacts.** The agent can create and publish Docs, Pages and Slides to the Tickloop Artifacts service and receive @tickloop feedback left on them. Sign in with `/artifacts`.
- **Persistent agents.** Agents run in a local daemon. Closing the terminal, a pane or `/exit` only leaves the interface; `/resume` and `tickloop -c` bring a Chat back.

## Requirements

- macOS on Apple silicon or Intel, or Linux on ARM64 or x86_64 (static builds that run on glibc and musl distributions).
- A terminal with true color and a UTF-8 locale. Terminals with the kitty keyboard protocol (Ghostty, kitty, WezTerm, iTerm2 with CSI u) get extra key bindings such as `ctrl+enter`.
- Optional: [Herdr](https://github.com/herdrdev/herdr) to show Ticks in panes.

## Install

`tickloop` and `tickhost` are always installed and upgraded together. The daemon only starts a `tickhost` from the same directory and build.

### Homebrew

```sh
brew install tickloopai/tap/tickloop
```

### Install script

```sh
curl -fsSL https://tickloop.ai/install.sh | sh
```

The script installs into `~/.local/bin` and `~/.local/share/tickloop/versions/`, picks the build for your platform, pins the release tag and verifies SHA-256. When `~/.local/bin` is not on PATH, it adds it once to your shell configuration (`~/.zshrc`, `~/.bashrc` and `~/.profile`, or fish's `conf.d`) and names the files it changed. It refuses to take over an existing installation from another source. The script's source is [scripts/install.sh](scripts/install.sh).

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

Use `tickloop -h` or `tickloop --help` for command-line options. In the Console, `/help` covers commands and keyboard shortcuts; `tickloop -p --help`, `tickloop acp --help` and `tickloop tick --help` explain those CLI entries.

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

### Noninteractive execution and ACP

```sh
tickloop -p "Summarize this project"                 # --print is an alias; --p is not supported
printf '%s' 'Explain this text' | tickloop -p -
tickloop -p "Review this change" --output-format json --timeout 300
tickloop -p "Run the checks" --output-format stream-json
tickloop acp                                        # ACP JSON-RPC over stdin/stdout
```

Configure the provider with `/provider` first. These clients use the same saved model, permission rules and Agent runtime as the Console, without opening a terminal UI. An unknown working directory is refused: trust it interactively first, or explicitly assert trust for this invocation with `--working-directory-is-trusted` (does not save trust).

Print mode defaults to Auto with human prompts denied, not Full Access. `text` writes only the final answer; `json` writes a versioned result; `stream-json` writes versioned JSONL events and a final result. Diagnostics go to stderr. Exit codes are 0 (success), 1 (execution/unknown outcome/cleanup failure), 2 (arguments), 3 (human input required), 124 (timeout), 130 (SIGINT), and 143 (SIGTERM). `--no-session-persistence` disables saving the new Chat; Print does not resume an existing Chat.

The initial managed profiles do not offer structured questions, Plan approvals, Subagents, independent/scheduled goals, Tick subscriptions or persistent Artifact feedback. Ordinary tool denials need not fail the whole task. A completed turn is settled and its remaining Shells and hosted Ticks are cleaned before success is reported; failures and unknown outcomes are not silently retried. Client crashes are covered by an expiring daemon control lease. Ordinary Console detach behavior is unchanged.

Configure an ACP-capable editor to launch `tickloop acp`. Stable ACP v1 supports successive prompts, text/tool updates, one-time permission answers, cancellation, resource references/embedded text, and inline images when the selected model supports them. Resource URIs are provenance, not permission to read or fetch them. Session-scoped configuration offers real models and Auto/Ask within the startup ceiling; advertised slash commands are actual user-invocable Skills. Persistent ACP sessions support listing and exclusive loading with public history replay; loading never runs a model, tool or MCP server and cannot take over a live Chat or ordinary Console history. Print still has no resume mode.

Client-provided stdio MCP servers are validated at session creation and start only on the first prompt, after mandatory one-time human approval (explicit Deny still wins, including in Full Access). Servers use a clean minimal environment plus explicitly supplied variables, and remain available across prompts until release/EOF/lease expiry. argv/env are not recorded in public history; argv is shown transiently to the supplying client for approval, so put secrets in env, not argv. MCP processes are not sandboxed; a supervisor cleans their process groups, but cannot guarantee cleanup of malicious `setsid` escapes or simultaneous supervisor termination. HTTP/SSE MCP and client filesystem/terminal execution are not supported.

A client's `cwd` is not itself a trust assertion. Prompts are bounded to 64 blocks, 256 KiB of text, 1 MiB of embedded resource text and four images (16 MiB aggregate base64); protocol frames and queues are also bounded. stdout is exclusively ACP protocol traffic. ACP is an AgentAccess client, not a Tick type.

### Keys

- `shift+tab` cycles execution modes. `enter` sends; while the agent works, the message is queued and joins at the next tool boundary. `ctrl+enter` interrupts and sends immediately.
- `ctrl+j` (or `alt+enter`/`shift+enter` where the terminal reports them) inserts a newline. `ctrl+g` edits the input in your external editor. Text input has no Vim mode.
- `esc esc` on an empty input opens Rewind. `ctrl+b` sends a foreground Bash command that has run for two seconds to the background.
- Lists and menus use `j`/`k` or `ctrl+n`/`ctrl+p`, `enter` to confirm and `esc` to go back one level.
- In the Monitor, `enter` opens a Tick or Shell view (a popup inside Herdr, full screen otherwise). In a view, `esc` hides it while the program keeps running, and `ctrl+b esc` sends Esc to the program. `s` stops a running Shell, and `x` closes or removes a row after asking.

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

Extension Ticks speak Tick protocol 3: line-delimited JSON-RPC 2.0 over stdin/stdout with `tick/describe`, `tick/read`, `tick/call` and the `tick/update` notification, while the program draws its interface on its controlling terminal. Any program can become a Tick by implementing it.

- [Tick protocol specification](tick/protocol/SPECIFICATION.en.md) ([中文](tick/protocol/SPECIFICATION.md))
- [Counter example](tick/examples/counter.py)
- Rust Tick-side SDK: [`tick/sdk`](tick/sdk)

## Documentation

- [Tick protocol specification](tick/protocol/SPECIFICATION.en.md)
- [Domain glossary](CONTEXT.md) (Chinese)
- [Contributing](CONTRIBUTING.md) and [agent working rules](CLAUDE.md) (Chinese)

## Contributing

Bug reports, ideas and pull requests are welcome. Read [CONTRIBUTING.md](CONTRIBUTING.md) for the build, test and review workflow. For new Tick packages or changes to the official ones, use [tickloopai/ticks](https://github.com/tickloopai/ticks).

## Security

Do not report vulnerabilities in public issues. See [SECURITY.md](SECURITY.md) for private reporting.

## License

Tickloop is released under the [MIT License](LICENSE). Adapted third-party code and its licenses are listed in [THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md).
