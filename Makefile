DEV_LAUNCHER := $(shell dirname '$(subst ','"'"',$(strip $(MAKEFILE_LIST)))')/scripts/dev-agent.sh
# Local development entry point. Put provider keys in .env at the repository root (ignored by Git), for example:
#   DEEPSEEK_API_KEY=sk-...
-include .env
export

ARGS ?=

CARGO_PACKAGE := -p tickloop-agent
PACKAGE_TARGET ?= $(shell rustc -vV | sed -n 's/^host: //p')
PACKAGE_ARCHIVE = dist/tickloop-$(PACKAGE_TARGET).tar.gz

.PHONY: dev build install package test test-workspace check check-workspace release

## Start in the current directory with global config/login/Tick packages and worktree+branch service isolation. Example: make dev ARGS="-c"
# Existing TICKLOOP_DAEMON_RUNTIME_DIR overrides the daemon (and default Tickhost subdirectory);
# TICKLOOP_TICKHOST_RUNTIME_DIR independently overrides Tickhost. Explicit overrides may share services.
# An explicit CARGO_TARGET_DIR gets a worktree+branch subdirectory to keep build artifacts isolated.
dev:
	"$(DEV_LAUNCHER)" $(ARGS)

build:
	cargo build --locked $(CARGO_PACKAGE) -p tickloop-tickhost -p tickloop-tick-reader --bins

install:
	cargo install --locked --path agent/tickloop --bins
	cargo install --locked --path tickhost/host --bin tickhost
	cargo install --locked --path tick/reader --bin tick-reader

## Package the Agent, the Tickhost and the Reader.
package:
	cargo build --locked --release $(CARGO_PACKAGE) -p tickloop-tickhost -p tickloop-tick-reader --bins --target "$(PACKAGE_TARGET)" --target-dir target
	@mkdir -p dist
	COPYFILE_DISABLE=1 tar --format=ustar -czf "$(PACKAGE_ARCHIVE)" -C "target/$(PACKAGE_TARGET)/release" tickloop tickhost tick-reader
	@printf 'Package: %s\n' "$(PACKAGE_ARCHIVE)"

test: test-workspace
	cargo test --locked --manifest-path distribution/cargo/Cargo.toml --target-dir target/cargo-launcher

## Workspace tests without the Cargo launcher, whose build downloads the matching release; CI runs
## these and the release workflow verifies the launcher. NEXTEST_PROFILE=ci retries flaky PTY tests.
test-workspace:
	@cargo nextest --version >/dev/null 2>&1 || { echo 'make test requires cargo-nextest; install it with: cargo install --locked cargo-nextest' >&2; exit 1; }
	node --test scripts/dev-agent.test.mjs
	@mkdir -p /tmp/tickloop-test
	TICKLOOP_CONFIG_DIR=$$(mktemp -d /tmp/tickloop-test/config.XXXXXX) \
	TICKLOOP_TMPDIR=$$(mktemp -d /tmp/tickloop-test/tmp.XXXXXX) \
	NO_PROXY=127.0.0.1,localhost no_proxy=127.0.0.1,localhost \
	env $$(env | grep -o '^HERDR_[A-Za-z0-9_]*' | sed 's/^/-u /') \
	cargo nextest run --locked --workspace --no-fail-fast

check: check-workspace
	cargo clippy --locked --manifest-path distribution/cargo/Cargo.toml --target-dir target/cargo-launcher --all-targets -- -D warnings
	cargo test --locked --manifest-path distribution/cargo/Cargo.toml --target-dir target/cargo-launcher

check-workspace:
	cargo fmt --all --check
	cargo fmt --manifest-path distribution/cargo/Cargo.toml -- --check
	cargo clippy --locked --workspace --all-targets -- -D warnings
	$(MAKE) test-workspace

## Bump the Tickloop patch version (VERSION=X.Y.Z names a higher one), check it, commit the version files and push the branch and tag.
release:
	@node scripts/release.mjs binary
