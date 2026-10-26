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
| `/ticks`, `/artifacts` | Add data sources; sign in to Artifacts |
| `/exit` | Leave the interface (the agent keeps running) |

Keys:

- `enter` sends a message. While the agent works it waits for the next step; `ctrl+enter` interrupts and sends right away.
- `esc` interrupts the agent. Press it twice on an empty input to undo to an earlier message.
- `shift+tab` switches between Auto and Plan mode.
- `ctrl+j` inserts a new line; `ctrl+g` edits your message in an external editor.
- In lists and menus, use `j`/`k` or the arrow keys, `enter` to choose and `esc` to go back.
- In the Monitor, `enter` opens a Tick and `esc` hides it again; the program keeps running.

## Instructions

The agent follows standing instructions from Markdown files, read in this order (later files weigh more):

1. Your own instructions for every project: `~/.tickloop/AGENTS.md`. Without it, `~/.claude/CLAUDE.md` and `~/.claude/rules/*.md` are used instead.
2. Each directory from the top of the filesystem down to the working directory: its `AGENTS.md`, or, in a directory without one, its `CLAUDE.md`, `.claude/CLAUDE.md` and `.claude/rules/*.md`. An `AGENTS.md` in a parent folder therefore applies to every project below it.
3. A directory's `CLAUDE.local.md`, your private notes that stay out of Git, is always read.

To keep a directory's `CLAUDE.md` from being read, put an `AGENTS.md` there; an empty one is enough. Likewise, any `~/.tickloop/AGENTS.md`, even an empty one, keeps `~/.claude` from being read. A rule file with `paths` in its frontmatter is left out. Write `@path` to bring in another file (`@docs/style.md`, `@~/notes.md`); a project's files can only bring in files inside the working directory, while your own instructions can bring in any file. Files are read when a conversation starts and again after `/compact`.

To reuse instructions written for another tool, bring them in from `~/.tickloop/AGENTS.md` (for example `@~/.codex/AGENTS.md`), or add an `AGENTS.md` to the project that brings in its existing file (for example `@GEMINI.md`).

## Data sources

Tick packages connect the agent to more data. Browse and install them with `/ticks`, or from the command line:

```sh
tickloop tick list --available
tickloop tick install PACKAGE
tickloop tick login PACKAGE
```

Installing a package only adds it: nothing runs and no access is granted until the agent opens it under your permission settings. Official packages live in [tickloopai/ticks](https://github.com/tickloopai/ticks).

Any program can become a Tick by speaking the [Tick protocol](tick/protocol/SPECIFICATION.md); start from the [counter example](tick/examples/counter.py).

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
