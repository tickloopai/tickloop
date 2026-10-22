# Contributing to Tickloop

Thanks for helping improve Tickloop. This guide covers the development setup, the checks a change must pass, and how the repository is organized.

## Where to contribute

- **Bugs and feature requests:** open a [GitHub issue](https://github.com/tickloopai/tickloop/issues/new/choose). Search existing issues first.
- **Security problems:** report them privately as described in [SECURITY.md](SECURITY.md), never in a public issue.
- **Tick packages** (the official ones or a new data source) live in [tickloopai/ticks](https://github.com/tickloopai/ticks), together with their packaging and the Tick marketplace. Package mechanics on the agent side (`tick/packages`, `agent/packages`, `/ticks`, `tickloop tick ...`) stay in this repository.
- **Larger changes:** open an issue to discuss the design before writing a lot of code, especially for changes to the Tick protocol, the permission system or process lifecycle.

## Development setup

- macOS (Apple silicon or Intel) or Linux (ARM64 or x86_64); CI checks both.
- The current stable Rust toolchain (edition 2024), with `rustfmt` and `clippy`.
- [cargo-nextest](https://nexte.st/): `cargo install --locked cargo-nextest`.
- Node.js 22 for the script tests.
- Optional: [Herdr](https://github.com/herdrdev/herdr) to exercise Tick panes.

```sh
make build     # build tickloop and tickhost
make dev       # build incrementally and start the agent in the current directory (ENV=prod, pre or dev)
make install   # install both programs into ~/.cargo/bin
make package   # build a release archive into dist/
```

`make dev` uses your normal configuration root (`~/.tickloop` or `TICKLOOP_CONFIG_DIR`), including logins and installed Tick packages, and isolates the daemon and Tickhost per worktree and branch. Model API keys can go in a `.env` file at the repository root (ignored by Git), for example `DEEPSEEK_API_KEY=...`.

`ENV` picks the Tickloop Cloud the agent talks to. `make dev` always sets the agent's `TICKLOOP_ARTIFACT_ORIGIN`, which also gives the Accounts issuer and the MCP endpoint, replacing any value from `.env` or your shell, and prints the choice before it builds:

```sh
make dev                    # ENV=prod, the default: https://tickloop.ai
make dev ENV=pre            # https://pre.tickloop.ai
make dev ENV=dev ARGS="-c"  # TICKLOOP_DEV_ARTIFACT_ORIGIN from .env
```

For `ENV=dev`, put `TICKLOOP_DEV_ARTIFACT_ORIGIN=<your dev cloud origin>` in `.env`; without it `make dev ENV=dev` stops before building. Only the Makefile reads this key, and the agent never sees it. Only an `ENV=` on the `make` command line counts: an `ENV` exported in your shell is ignored and is not passed to the agent.

The daemon only starts a `tickhost` built from the same sources. When a View meets a daemon or Tickhost from another build, it asks whether to replace it. To replace a stale development daemon by hand:

```sh
./target/debug/tickloop daemon stop
```

This stops every running agent in that daemon. Saved history is kept, and Ticks in Tickhost keep running.

## Checks

CI (`.github/workflows/ci.yml`) does not start on pushes or pull requests: it runs as part of every release, and maintainers can start it by hand from the Actions tab. Run the same checks locally before opening a pull request:

```sh
make check-workspace
```

It runs `cargo fmt --all --check`, `cargo clippy --locked --workspace --all-targets -- -D warnings` and `make test-workspace`, which runs the whole workspace under cargo-nextest (configuration in `.config/nextest.toml`) with isolated state directories and without inherited `HERDR_*` variables; CI uses the nextest `ci` profile, which retries timing-sensitive PTY tests. `make check` and `make test` also cover the Cargo launcher in `distribution/cargo`, whose build downloads the release matching its version, so they need network access and fail on a version that is not released yet. CI also runs `node --test` on the release and install scripts.

### Writing tests

- Each crate has exactly one integration test binary: `tests/all.rs` plus modules in `tests/all/`.
- Put tests in the crate that owns the code. Only tests that need the full executable or `cli_fixture` belong in `agent/tickloop/tests`.
- Prefer testing from the agent's tools and permissions, and from Tick subscriptions driving the Loop.
- Protocol tests use real child processes and PTYs. Host tests use controllable adapters and an isolated Herdr.
- Tests must only clean up resources they created.
- Tests never need real model or data-source accounts. Tests against live APIs stay behind a feature flag and are ignored by default.

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
| `tick/packages` | Tick package manifests, marketplace index, installation and credential files |
| `tick/oauth` | Standard OAuth 2.0 client used for Tick package logins |
| `tick/oauth-test-server` | Test-only OAuth 2.0 authorization server shared by Tick and agent login tests |
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

`tickloop` and `tickhost` share one product version, the workspace version in the root `Cargo.toml`. Tick packages are versioned independently in tickloopai/ticks.

```sh
make release                          # bump the patch version and publish from this Mac
make release VERSION=X.Y.Z            # or pick a higher version
make release REMOTE=1                 # publish on GitHub Actions instead
make release-publish VERSION=X.Y.Z    # publish a pushed tag again; FROM=step resumes, REMOTE=1 uses Actions
```

`make release` checks a temporary checkout of `HEAD`, creates a dedicated `chore(release): ...` commit and an annotated `vX.Y.Z` tag, and pushes the branch and tag. It then publishes the tag from this Mac with `scripts/release-publish.mjs`, in the same stages as `.github/workflows/release.yml`:

1. `prepare`: the crates.io package audit, and access to the tap, crates.io, Docker and Rosetta;
2. `build`: the macOS archives natively and the static musl archives in Linux containers (`scripts/release/linux.Dockerfile`, x86_64 under emulation), each smoke-tested;
3. `publish`: the archives as a release in [tickloopai/homebrew-tap](https://github.com/tickloopai/homebrew-tap);
4. `tap`: the Formula;
5. `cargo`: the crates.io launcher, verified on macOS and Linux.

Publishing does not run the tests: run `make check` (and the Linux checks when a change touches platform code) before `make release`.

It needs an Apple silicon Mac with Rosetta, Docker (OrbStack), `gh` signed in with push access to the tap, and `cargo login` for crates.io. The tagged source is checked out in `target/release-local/source`, and the archives are kept in `target/release-local/vX.Y.Z/`. `REMOTE=1` starts `release.yml` on GitHub Actions for the pushed tag instead; tags no longer start it on their own.

Assets and crates for an existing version are never replaced with different content. When a stage fails after the tag was pushed, fix the cause and resume with `make release-publish VERSION=X.Y.Z FROM=<stage>` (or rerun the failed jobs on GitHub) instead of bumping again. If pushing the tag fails after the release commit was created, push the missing refs instead of running `make release` again.
