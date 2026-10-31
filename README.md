# Tickloop

[Website](https://tickloop.ai) · English | [简体中文](README.zh-CN.md)

[![crates.io](https://img.shields.io/crates/v/tickloop)](https://crates.io/crates/tickloop)
[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](LICENSE)

Tickloop is an AI agent that works in your terminal. Tell it what you need in plain language: it reads your project, edits code, runs tests, organizes material, writes documents and builds pages or slide decks, and keeps going until the job is done.

## Why Ticks

What makes it different are **Ticks**: live programs, such as a log viewer, a dashboard or a long-running command, that you and the agent watch together. The agent can read a Tick, use it, and be woken up when something in it changes.

Install them from [Ticks](docs/ticks.md), or speak the protocol yourself ([Writing Ticks](docs/writing-ticks.md)). The quickstart below asks the agent to open the [counter example](tick/examples/counter.py).

## Features

- **Hand off real work.** The agent edits files, runs commands and checks the results. By default it works in Auto mode, where a reviewer model approves routine actions and asks you about risky ones. [Permissions](docs/permissions.md)
- **Keep an eye on things.** Open a program as a Tick and subscribe to it; the agent reacts when it changes. The Monitor beside the conversation lists your Ticks, background commands and scheduled tasks. [Ticks](docs/ticks.md) · [Interface](docs/interface.md)
- **Repeat work on a schedule.** `/loop 5m check the deploy` runs an instruction every five minutes; `/goal` sets a condition the agent keeps working toward.
- **Use the model you like.** Claude, OpenAI (API or ChatGPT subscription), GitHub Copilot, Gemini, DeepSeek, Kimi, MiniMax, GLM, xAI and Alibaba are built in. Switch at any time with `/model`.
- **Share results.** Publish docs, pages and slides to Tickloop Artifacts and get feedback from others. `/artifacts` signs in and adds them to the Chat.
- **Leave and come back.** Closing the terminal does not stop the agent. `tickloop -c` picks up the latest conversation. [Agents overview](docs/interface.md#agents-overview)

## Requirements

- macOS (Apple silicon or Intel) or Linux (ARM64 or x86_64).
- A terminal with true color and UTF-8. Ghostty, kitty, WezTerm or iTerm2 enable extra keys such as `ctrl+enter`.

Settings, logins and history are kept in `~/.tickloop`.

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

## Quickstart

1. Run `tickloop` in your project folder.
2. On first launch, pick a model provider, enter its API key or sign in, then choose a model.
3. Describe what you want done.

Want to see a Tick in action? Ask the agent:

> Open `python3 tick/examples/counter.py` with `protocol: true` and subscribe to it.

Then select the Counter in the Monitor and press Enter to step into its card, type `+` and press Enter: the agent receives the new count and responds. Press `esc` twice to step out again.

## Common commands

Use `tickloop --help` for CLI options.

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
| `/ticks`, `/plugins` | Install and log in to Ticks; manage other plugins |
| `/mcp` | Inspect MCP servers, manage their login, change their exposure and turn them on or off here |
| `/hooks` | See this folder's hooks; trust, disable or restore them |
| `/context` | See what fills the context, by category, and how many MCP tools tool search can reach; `/context all` lists every item |
| `/artifacts` | Sign in to Artifacts and add artifacts to the Chat |
| `/exit` | Leave the interface (the agent keeps running) |

- `enter` sends a message. While the agent works it waits for the next step; `ctrl+enter` interrupts and sends right away.
- `esc` interrupts the agent. Press it twice on an empty input to undo to an earlier message.
- `shift+tab` switches between Auto and Plan mode.
- `ctrl+j` inserts a new line; `ctrl+g` edits your message in an external editor.
- In lists and menus, use `ctrl+n`/`ctrl+p` or the arrow keys, `enter` to choose and `esc` to go back.

The Monitor, full-screen Ticks and Shells, `tickloop attach` and the Agents overview are in [Interface](docs/interface.md).

## Modes

The mode decides how the agent gets permission to act. **Auto** (the default) lets a reviewer model approve routine actions and asks you about risky ones; **Ask** allows reads and edits in the working directory and asks about other protected operations; **Plan** only plans; **Full Access** skips ordinary approval but keeps explicit Ask and Deny rules. Switch with `/permissions`, `/mode` or `shift+tab`. Launch rules, extra directories and importing from Claude Code are in [Permissions](docs/permissions.md). On macOS, Bash and Shells can run in a [sandbox](docs/sandbox.md).

## Beyond the terminal

```sh
tickloop -p "Summarize this project"    # --print is an alias; --p is not supported
```

```sh
tickloop acp                            # ACP JSON-RPC over stdin/stdout
```

Print and ACP use the same saved model, permission rules and Agent runtime as the Console, without opening a terminal UI. Output formats, exit codes and editor setup are in [Print and ACP](docs/print-and-acp.md).

## Documentation

| Guide | What it covers |
| --- | --- |
| [Interface](docs/interface.md) | Keys, the Monitor, the Agents overview, context compaction |
| [Permissions](docs/permissions.md) | Execution modes, launch rules, additional directories, Claude Code import |
| [Sandbox](docs/sandbox.md) | macOS sandbox for Bash and Shells |
| [Print and ACP](docs/print-and-acp.md) | Noninteractive execution and editor integration |
| [Instructions](docs/instructions.md) | `AGENTS.md`, `CLAUDE.md` and `@path` includes |
| [Ticks](docs/ticks.md) | Install, log in and open Ticks |
| [Writing Ticks](docs/writing-ticks.md) | Protocol, width and keys |
| [Plugins](docs/plugins.md) | Plugins, marketplaces, Skills, commands, user configuration |
| [MCP](docs/mcp.md) | MCP servers, tool search, login |
| [Hooks](docs/hooks.md) | Command hooks and trust |
| [Configuration](docs/configuration.md) | Every `settings.json` key in this manual |

## Contributing

Bug reports, ideas and pull requests are welcome; see [CONTRIBUTING.md](CONTRIBUTING.md). To build from source, see its development setup.

## Security

Please report vulnerabilities privately as described in [SECURITY.md](SECURITY.md), not in public issues.

## License

[MIT](LICENSE). Third-party code and licenses are listed in [THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md).
