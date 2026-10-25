# Contributing to Tickloop

Thanks for helping improve Tickloop. This guide covers the development setup, the checks a change must pass, and how the repository is organized.

## Where to contribute

- **Bugs and feature requests:** open a [GitHub issue](https://github.com/tickloopai/tickloop/issues/new/choose). Search existing issues first.
- **Security problems:** report them privately as described in [SECURITY.md](SECURITY.md), never in a public issue.
- **Tick packages** (the official ones or a new data source) live in [tickloopai/ticks](https://github.com/tickloopai/ticks), together with their packaging and the Tick marketplace. Package mechanics on the agent side (`tick/packages`, `tick/installer`, `/ticks`, `tickloop tick ...`) stay in this repository.
- **Larger changes:** open an issue to discuss the design before writing a lot of code, especially for changes to the Tick protocol, the permission system or process lifecycle.

## Development setup

- macOS (Apple silicon or Intel) or Linux (ARM64 or x86_64); CI checks both.
- The current stable Rust toolchain (edition 2024), with `rustfmt` and `clippy`.
- [cargo-nextest](https://nexte.st/): `cargo install --locked cargo-nextest`.
- Node.js 22 for the script tests.
- For public API checks: `rustup toolchain install nightly-2026-10-19 --profile minimal` and `cargo install --locked cargo-public-api --version 0.52.0`.
- Optional: [Herdr](https://github.com/herdrdev/herdr) to exercise Tick panes.

```sh
make build     # build tickloop and tickhost
make dev       # build incrementally and start the agent in the current directory (ENV=prod, pre or dev)
make install   # install both programs into ~/.cargo/bin
make package   # build a release archive into dist/
```

`make dev` copies this checkout's build into `target/dev-runs/` and registers it as a development runtime. It uses your normal configuration root (`~/.tickloop` or `TICKLOOP_CONFIG_DIR`), including logins and installed Tick packages, and the same daemon and Tickhost as an installed `tickloop`. Model API keys can go in a `.env` file at the repository root (ignored by Git), for example `DEEPSEEK_API_KEY=...`.

`ENV` picks the Tickloop Cloud the agent talks to. `make dev` always sets the agent's `TICKLOOP_ARTIFACT_ORIGIN`, which also gives the Accounts issuer and the MCP endpoint, replacing any value from `.env` or your shell, and prints the choice before it builds:

```sh
make dev                    # ENV=prod, the default: https://tickloop.ai
make dev ENV=pre            # https://pre.tickloop.ai
make dev ENV=dev ARGS="-c"  # TICKLOOP_DEV_ARTIFACT_ORIGIN from .env
```

For `ENV=dev`, put `TICKLOOP_DEV_ARTIFACT_ORIGIN=<your dev cloud origin>` in `.env`; without it `make dev ENV=dev` stops before building. Only the Makefile reads this key, and the agent never sees it. Only an `ENV=` on the `make` command line counts: an `ENV` exported in your shell is ignored and is not passed to the agent.

A View works with any running daemon that speaks a compatible protocol, which may be from another build. Only when the running daemon's protocol is incompatible does the View offer to stop it and open this build, open the running build instead, or cancel. An incompatible Tickhost keeps running, with its Ticks, until you stop it with `tickhost stop`. To stop the daemon by hand:

```sh
tickloop daemon stop
```

This stops every running agent in that daemon. Saved history is kept; extension and Terminal Ticks keep running in Tickhost, while background Shells end with their agents.

## Checks

CI (`.github/workflows/ci.yml`) does not start on pushes, pull requests or releases; maintainers start it by hand from the Actions tab. Before opening a pull request, run locally:

```sh
make check-workspace
```

It first checks dependency boundaries and the Tick public API, then runs `cargo fmt --all --check` (and the same check for the Cargo launcher in `distribution/cargo`), `cargo clippy --locked --workspace --all-targets --all-features -- -D warnings` (including every facade export) and `make test-workspace`. The last runs the development and boundary-check script tests with `node --test` and the whole workspace under cargo-nextest (configuration in `.config/nextest.toml`), with isolated state directories and without inherited `HERDR_*` variables. CI runs the same target with the nextest `ci` profile, which retries timing-sensitive PTY tests, and also runs `node --test` on the release and install scripts and, on Linux, clippy for the static musl target.

`scripts/dependency-boundaries.json` classifies every workspace crate. `node scripts/check-boundaries.mjs` reads `cargo metadata`, checking normal, build and dev dependencies on every target. Transitional exceptions name an exact edge, dependency kind, target and reason; remove them as the corresponding migration lands. New packages require an explicit layer, new forbidden edges fail, and obsolete exceptions fail too.

`node scripts/check-public-api.mjs` compares ids, protocol, SDK, packages, OAuth and ui against the immutable release recorded in `scripts/public-api-baseline.json`. It exports that release into an isolated directory and generates baseline and current snapshots with `cargo public-api`, all features, the same nightly toolchain and native target. Added, changed and removed signatures all fail. Snapshots remain under `target/public-api/check-*`; no checkout or existing worktree is changed. The release tag must be available locally (CI fetches full history). Update the recorded release and commit only when intentionally accepting a new public API baseline. Both versions are rebuilt on each run so platform and toolchain differences cannot masquerade as source changes.

`make test` adds the Cargo launcher's tests, and `make check` its clippy and tests, to the workspace checks. The launcher's build downloads the release matching its version, so these targets need network access and fail on a version that is not released yet; maintainers run them before a release.

### Writing tests

- Each crate has at most one integration test binary, `all`: `tests/all.rs` (or `tests/all/main.rs`) plus modules in `tests/all/`. `agent/tickloop` also builds the `cli_fixture` program its tests call.
- Put tests in the crate that owns the code. Only tests that need the full executable or `cli_fixture` belong in `agent/tickloop/tests`.
- Prefer testing from the agent's tools and permissions, and from Tick subscriptions driving the Loop.
- Protocol tests use real child processes and PTYs. Host tests use controllable adapters and an isolated Herdr.
- Tests must only clean up resources they created.
- Tests never need real model or data-source accounts. Tests against live services are marked `#[ignore]` and run only on request.

## Repository layout

The agent lives in `agent/` (the `tickloop` executable, Console, client, daemon, runtime and their support crates), the Tick host in `tickhost/` (the `tickhost` executable and its client), the Tick standard, SDK, package and OAuth crates and examples in `tick/`, shared terminal presentation in `ui/`, the crates.io launcher in `distribution/cargo/` and a patched crossterm copy in `vendor/crossterm/` (see `vendor/crossterm/UPSTREAM.md`). The per-crate table and the dependency boundaries are in [AGENTS.md](AGENTS.md) under "Structure and integration boundaries".

## Design documents and conventions

- Domain terms follow [CONTEXT.md](CONTEXT.md). Read it before designing a change, and use its terms in code and discussion. If your change conflicts with documented behavior, say so in the pull request.
- [`AGENTS.md`](AGENTS.md) holds the working rules for coding agents and doubles as a compact summary of the core behavior contracts. Design records and ADRs are kept by the maintainers outside the repository; ask in an issue when you need the background of a decision.
- Interaction conventions for input, lists, menus and Tick views are in [AGENTS.md](AGENTS.md) under "Interaction rules".
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

Publishing does not run the tests: run `make check` (`make check-workspace` plus the Cargo launcher's clippy and tests; add the Linux checks when a change touches platform code) before `make release`.

It needs an Apple silicon Mac with Rosetta, Docker (OrbStack), `gh` signed in with push access to the tap, and `cargo login` for crates.io. The tagged source is checked out in `target/release-local/source`, and the archives are kept in `target/release-local/vX.Y.Z/`. `REMOTE=1` starts `release.yml` on GitHub Actions for the pushed tag instead; tags no longer start it on their own.

Assets and crates for an existing version are never replaced with different content. When a stage fails after the tag was pushed, fix the cause and resume with `make release-publish VERSION=X.Y.Z FROM=<stage>` (or rerun the failed jobs on GitHub) instead of bumping again. If pushing the tag fails after the release commit was created, push the missing refs instead of running `make release` again.
