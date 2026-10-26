DEV_LAUNCHER := $(shell dirname '$(subst ','"'"',$(strip $(MAKEFILE_LIST)))')/scripts/dev-agent.sh
# Local development entry point. Put provider keys in .env at the repository root (ignored by Git), for example:
#   DEEPSEEK_API_KEY=sk-...
-include .env
export

ARGS ?=

# The Tickloop Cloud that make dev talks to: prod (the default), pre, or dev, whose origin is the
# Makefile-only .env key TICKLOOP_DEV_ARTIFACT_ORIGIN; the Agent never reads that name. make dev
# always sets the Agent's TICKLOOP_ARTIFACT_ORIGIN, which also gives the Accounts issuer and the MCP
# endpoint. Only a command-line `ENV=...` selects it; an exported shell variable of the same name
# is ignored so a leftover export cannot redirect the Agent, and it is never passed on, since sh
# reads $ENV as a startup file.
ENV := prod
unexport ENV TICKLOOP_DEV_ARTIFACT_ORIGIN
DEV_ENVIRONMENTS := prod pre dev
DEV_CLOUD_ORIGIN_prod := https://tickloop.ai
DEV_CLOUD_ORIGIN_pre := https://pre.tickloop.ai
DEV_CLOUD_ORIGIN_dev := $(TICKLOOP_DEV_ARTIFACT_ORIGIN)
DEV_ENV_IS_KNOWN = $(and $(filter 1,$(words $(ENV))),$(filter $(DEV_ENVIRONMENTS),$(ENV)))

CARGO_PACKAGE := -p tickloop-code
PACKAGE_TARGET ?= $(shell rustc -vV | sed -n 's/^host: //p')
PACKAGE_ARCHIVE = dist/tickloop-$(PACKAGE_TARGET).tar.gz

.PHONY: dev build install package test test-workspace check check-workspace release release-publish

## Start this checkout's build with its own ~/.tickloop-dev config root, daemon and Tickhost
## (override with TICKLOOP_DEV_CONFIG_DIR), against Tickloop Cloud ENV=prod (default), ENV=pre or ENV=dev.
## Example: make dev ENV=pre ARGS="-c"
# ENV=dev needs TICKLOOP_DEV_ARTIFACT_ORIGIN=<your dev cloud origin> in .env. Every ENV replaces any
# TICKLOOP_ARTIFACT_ORIGIN from .env or the environment.
dev: override TICKLOOP_ARTIFACT_ORIGIN := $(DEV_CLOUD_ORIGIN_$(ENV))
dev:
	$(if $(DEV_ENV_IS_KNOWN),,$(error Unknown ENV '$(ENV)'; use ENV=prod (the default), ENV=pre or ENV=dev))
	$(if $(TICKLOOP_ARTIFACT_ORIGIN),,$(error ENV=dev needs your dev Tickloop Cloud: put TICKLOOP_DEV_ARTIFACT_ORIGIN=<your dev cloud origin> in .env))
	@printf 'Tickloop Cloud: %s (ENV=%s)\n' "$$TICKLOOP_ARTIFACT_ORIGIN" '$(ENV)' >&2
	"$(DEV_LAUNCHER)" $(ARGS)

build:
	cargo build --locked $(CARGO_PACKAGE) -p tickloop-tickhost --bins

install:
	cargo install --locked --force --path code/cli --bins
	cargo install --locked --path tickhost/host --bin tickhost

## Package the Agent and the Tickhost.
package:
	cargo build --locked --release $(CARGO_PACKAGE) -p tickloop-tickhost --bins --target "$(PACKAGE_TARGET)" --target-dir target
	@mkdir -p dist
	COPYFILE_DISABLE=1 tar --format=ustar -czf "$(PACKAGE_ARCHIVE)" -C "target/$(PACKAGE_TARGET)/release" tickloop tickhost
	@printf 'Package: %s\n' "$(PACKAGE_ARCHIVE)"

test: test-workspace
	cargo test --locked --manifest-path distribution/cargo/Cargo.toml --target-dir target/cargo-launcher

## Workspace tests without the Cargo launcher, whose build downloads the matching release; CI runs
## these and the release workflow verifies the launcher. NEXTEST_PROFILE=ci retries flaky PTY tests.
test-workspace:
	@cargo nextest --version >/dev/null 2>&1 || { echo 'make test requires cargo-nextest; install it with: cargo install --locked cargo-nextest' >&2; exit 1; }
	node --test scripts/test-workspace.test.mjs scripts/dev-agent.test.mjs scripts/check-boundaries.test.mjs scripts/check-public-api.test.mjs scripts/plugin-compat-report.test.mjs
	node scripts/test-workspace.mjs


check: check-workspace
	cargo clippy --locked --manifest-path distribution/cargo/Cargo.toml --target-dir target/cargo-launcher --all-targets -- -D warnings
	cargo test --locked --manifest-path distribution/cargo/Cargo.toml --target-dir target/cargo-launcher

check-workspace:
	node --test scripts/check-agent-access.test.mjs
	node scripts/check-agent-access.mjs
	node scripts/check-boundaries.mjs
	node scripts/check-public-api.mjs
	cargo fmt --all --check
	cargo fmt --manifest-path distribution/cargo/Cargo.toml -- --check
	cargo clippy --locked --workspace --all-targets --all-features -- -D warnings
	$(MAKE) test-workspace

## Bump the Tickloop patch version (VERSION=X.Y.Z names a higher one), check it, commit the version files and push the branch and tag,
## then check, build and publish the release on this Mac (Docker for Linux); REMOTE=1 publishes on GitHub Actions instead.
release:
	@TICKLOOP_RELEASE_PUBLISH=$(if $(REMOTE),remote,local) node scripts/release.mjs binary

## Publish the pushed tag vVERSION again after a failure; FROM=step resumes at prepare, check, build, publish, tap or cargo.
release-publish:
	@test -n '$(VERSION)' || { echo 'Usage: make release-publish VERSION=X.Y.Z [FROM=step] [REMOTE=1]' >&2; exit 2; }
	@node scripts/release-publish.mjs $(if $(REMOTE),--remote) $(if $(FROM),--from '$(FROM)') 'v$(VERSION)'
