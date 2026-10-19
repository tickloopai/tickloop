# Contributing to Tickloop

Thanks for helping improve Tickloop. This guide covers the development setup, the checks a change must pass, and how the repository is organized.

## Where to contribute

- **Bugs and feature requests:** open a [GitHub issue](https://github.com/tickloopai/tickloop/issues/new/choose). Search existing issues first.
- **Security problems:** report them privately as described in [SECURITY.md](SECURITY.md), never in a public issue.
- **Tick packages** (`longbridge`, `futu`, or a new data source) live in [tickloopai/ticks](https://github.com/tickloopai/ticks), together with their packaging and the Tick marketplace. Package mechanics on the agent side (`tick/packages`, `agent/packages`, `/ticks`, `tickloop tick ...`) stay in this repository.
- **Larger changes:** open an issue to discuss the design before writing a lot of code, especially for changes to the Tick protocol, the permission system or process lifecycle.

## Development setup

- macOS (Apple silicon or Intel), the current target platform.
- The current stable Rust toolchain (edition 2024), with `rustfmt` and `clippy`.
- [cargo-nextest](https://nexte.st/): `cargo install --locked cargo-nextest`.
- Node.js 22 for the script tests.
- Optional: [Herdr](https://github.com/herdrdev/herdr) to exercise Tick panes.

```sh
make build     # build tickloop, tickhost and tick-reader
make dev       # build incrementally and start the agent in the current directory
make install   # install the three programs into ~/.cargo/bin
make package   # build a release archive into dist/
```

`make dev` uses your normal configuration root (`~/.tickloop` or `TICKLOOP_CONFIG_DIR`), including logins and installed Tick packages, and isolates the daemon and Tickhost per worktree and branch. Model API keys can go in a `.env` file at the repository root (ignored by Git), for example `DEEPSEEK_API_KEY=...`.

The daemon only starts a `tickhost` built from the same sources. When a View meets a daemon or Tickhost from another build, it asks whether to replace it. To replace a stale development daemon by hand:

```sh
./target/debug/tickloop daemon stop
```

This stops every running agent in that daemon. Saved history is kept, and Ticks in Tickhost keep running.

## Checks

Every pull request must pass the same checks CI runs (`.github/workflows/ci.yml`):

```sh
cargo fmt --all --check
cargo clippy --locked --workspace --all-targets -- -D warnings
make test
```

`make check` runs all of them, plus the same checks for the Cargo launcher in `distribution/cargo`. `make test` runs the whole workspace under cargo-nextest (configuration in `.config/nextest.toml`) with isolated state directories and without inherited `HERDR_*` variables. CI also runs `node --test` on the release and install scripts.

### Writing tests

- Each crate has exactly one integration test binary: `tests/all.rs` plus modules in `tests/all/`.
- Put tests in the crate that owns the code. Only tests that need the full executable or `cli_fixture` belong in `agent/tickloop/tests`.
- Prefer testing from the agent's tools and permissions, and from Tick subscriptions driving the Loop.
- Protocol tests use real child processes and PTYs. Host tests use controllable adapters and an isolated Herdr.
- Tests must only clean up resources they created.
- Tests never need real model or brokerage accounts. Tests against live APIs stay behind a feature flag and are ignored by default.

## Repository layout

| Path | Contents |
| --- | --- |
| `agent/tickloop` | The `tickloop` executable: Console, `daemon`, `worker`, `monitor` and `tick` subcommands, plus end-to-end tests |
| `agent/console` | Agent Console (terminal UI, Monitor, `/ticks`); reaches agents only through AgentAccess |
| `agent/client` | AgentAccess client: Unix socket transport, reconnection, finding or starting the local daemon |
| `agent/daemon` | Daemon and worker processes: AgentAccess server, embedded agent, worker supervision |
| `agent/runtime` | Agent runtime: Loops, Subagents, Transcript storage, Tick subscription streams, host tools, MCP |
| `agent/protocol` | All agent wire types and the AgentAccess contract |
| `agent/transcript` | Transcript record format and display projection |
| `agent/model` | Model clients, provider sign-in and typed judgments; built-in provider catalog in `agent/model/src/providers/` |
| `agent/tools` | Tools, permission rules and file change records |
| `agent/config` | User settings and the Skill registry, including built-in Skills |
| `agent/artifacts` | Client of the Tickloop Artifacts service: sign-in, credentials, catalog |
| `agent/packages` | Tick marketplace download, install, update, uninstall, login and logout |
| `agent/updater` | Versioned installation, update checks and pinned executable bundles |
| `agent/recurrence` | Wall-clock recurrence rules for Loop triggers |
| `agent/base` | Shared low-level utilities: private files, HTTP policy, images, output styles |
| `agent/test-support` | Test helpers shared by agent crates: scripted model, transcript stores, PTY screens, fake HTTP servers |
| `tickhost/host` | The `tickhost` executable: runs protocol, Terminal and Shell Ticks with multiple attached views |
| `tickhost/connect` | Tickhost business channel and client, per-instance Tick sessions, launch contract, program resolution, Herdr pane layout |
| `tick/protocol`, `tick/sdk` | The Tick standard and the Tick-side protocol implementation |
| `tick/command` | Shared entry point for Tick Commands |
| `tick/reader` | The `tick-reader` executable, bundled with Tickloop |
| `tick/market`, `tick/provider` | Financial Tick views and the provider interfaces that Tick packages implement |
| `tick/packages` | Tick package manifests, marketplace index, installation and credential files |
| `tick/oauth` | Standard OAuth 2.0 client used for Tick package logins |
| `tick/test-support` | Mock data, the test-only `tick-mock` and a fake OAuth server |
| `tick/examples` | Runnable examples of external protocol programs |
| `ui` | Shared terminal presentation, input and formatting rules |
| `distribution/cargo` | The crates.io launcher crate `tickloop` |
| `vendor/crossterm` | Patched crossterm copy (see `vendor/crossterm/UPSTREAM.md`) |

## Design documents and conventions

- Domain terms follow [CONTEXT.md](CONTEXT.md) (Chinese). Read it before designing a change, and use its terms in code and discussion. If your change conflicts with documented behavior, say so in the pull request.
- [`CLAUDE.md`](CLAUDE.md) (also linked as `AGENTS.md`, Chinese) holds the working rules for coding agents and doubles as a compact summary of the core behavior contracts. Design records and ADRs are kept by the maintainers outside the repository; ask in an issue when you need the background of a decision.
- Interaction conventions: lists and menus use `j`/`k` and `Ctrl+N`/`Ctrl+P`, Enter confirms and Esc backs out one level. Text input has no Vim mode. Menus show no numbers and accept no hidden number shortcuts. See `CLAUDE.md` for the full set.
- Keep comments for non-obvious logic; prefer clear names and small, focused functions.
- `.scratch/` is ignored by Git. It is a local workspace for maintainers' specs, tickets and evidence and is not part of the project. Public discussion happens in GitHub issues and pull requests.

## Pull requests

- Keep each pull request focused on one change and describe what it does and why.
- Use [Conventional Commits](https://www.conventionalcommits.org/) for commit messages, optionally scoped: `feat(runtime): ...`, `fix(tickhost): ...`, `docs: ...`. Types: `feat`, `fix`, `refactor`, `docs`, `test`, `chore`, `perf`, `ci`.
- Update `README.md` and `README.zh-CN.md` when user-visible behavior changes, and `CONTEXT.md` when a domain term changes.
- Fill in the test plan in the pull request template.

By contributing you agree that your contributions are licensed under the [MIT License](LICENSE).

## Releases (maintainers)

`tickloop`, `tickhost` and `tick-reader` share one product version, the workspace version in the root `Cargo.toml`. Tick packages are versioned independently in tickloopai/ticks.

```sh
make release                 # bump the patch version
make release VERSION=X.Y.Z   # or pick a higher version
```

`make release` checks a temporary checkout of `HEAD`, creates a dedicated `chore(release): ...` commit and an annotated `vX.Y.Z` tag, and pushes the branch and tag. The tag triggers `.github/workflows/release.yml`, which builds the macOS archives, publishes them as a draft release in [tickloopai/homebrew-tap](https://github.com/tickloopai/homebrew-tap), then publishes the release, updates the Formula and publishes the matching crates.io launcher. Assets and crates for an existing version are never replaced with different content; rerun failed jobs instead of bumping again. If pushing the tag fails after the release commit was created, push the missing refs instead of running `make release` again.
