DEV_LAUNCHER := $(shell dirname '$(subst ','"'"',$(strip $(MAKEFILE_LIST)))')/scripts/dev-agent.sh
# 本地开发入口。密钥放在仓库根目录的 .env（已被 .gitignore 忽略），例如：
#   DEEPSEEK_API_KEY=sk-...
-include .env
export

ARGS ?=

CARGO_PACKAGE := -p tickloop-agent
PACKAGE_TARGET ?= $(shell rustc -vV | sed -n 's/^host: //p')
PACKAGE_ARCHIVE = dist/tickloop-$(PACKAGE_TARGET).tar.gz
TICK_PACKAGES ?= $(notdir $(patsubst %/tick-package.json,%,$(wildcard ticks/*/tick-package.json)))

.PHONY: dev build install package site test check release prototype-earnings-review package-ticks dev-ticks release-ticks

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

## 在本地构建官网，输出到 site/dist
site:
	npm --prefix site ci
	npm --prefix site run build

## Build the Tick packages for Apple Silicon into dist/ticks and print their marketplace.json entries.
package-ticks:
	scripts/package-ticks.sh $(TICK_PACKAGES)

## Package the Tick packages, then install each with the tickloop built from this checkout.
dev-ticks:
	@TICK_PACKAGE_QUIET=1 scripts/package-ticks.sh $(TICK_PACKAGES)
	@echo "Building tickloop…"
	@cargo build --locked --quiet -p tickloop-agent --bin tickloop
	@for package in $(TICK_PACKAGES); do \
		cargo run --locked --quiet -p tickloop-agent --bin tickloop -- tick install --path "dist/ticks/$$package" || exit 1; \
	done

## Bump each selected Tick package, commit their versions together and push individual tags; select with TICK_PACKAGES="longbridge futu".
release-ticks:
	@node scripts/release.mjs ticks

test:
	@cargo nextest --version >/dev/null 2>&1 || { echo 'make test requires cargo-nextest; install it with: cargo install --locked cargo-nextest' >&2; exit 1; }
	node --test scripts/dev-agent.test.mjs
	@mkdir -p /tmp/tickloop-test
	TICKLOOP_CONFIG_DIR=$$(mktemp -d /tmp/tickloop-test/config.XXXXXX) \
	TICKLOOP_TMPDIR=$$(mktemp -d /tmp/tickloop-test/tmp.XXXXXX) \
	NO_PROXY=127.0.0.1,localhost no_proxy=127.0.0.1,localhost \
	env $$(env | grep -o '^HERDR_[A-Za-z0-9_]*' | sed 's/^/-u /') \
	cargo nextest run --locked --workspace --no-fail-fast
	cargo test --locked --manifest-path distribution/cargo/Cargo.toml --target-dir target/cargo-launcher

check:
	cargo fmt --all --check
	cargo fmt --manifest-path distribution/cargo/Cargo.toml -- --check
	cargo clippy --locked --workspace --all-targets -- -D warnings
	cargo clippy --locked --manifest-path distribution/cargo/Cargo.toml --target-dir target/cargo-launcher --all-targets -- -D warnings
	$(MAKE) test

## Tickloop 版本 patch +1，检查后提交版本文件并推送分支与标签；VERSION 可指定更高版本
release:
	@node scripts/release.mjs binary

## 启动财报复盘原型；访问 http://127.0.0.1:8766/earnings-review.html?variant=A
prototype-earnings-review:
	python3 -m http.server 8766 --directory .scratch/earnings-review/prototype
