DEV_LAUNCHER := $(shell dirname '$(subst ','"'"',$(strip $(MAKEFILE_LIST)))')/scripts/dev-agent.sh
# 本地开发入口。密钥放在仓库根目录的 .env（已被 .gitignore 忽略），例如：
#   DEEPSEEK_API_KEY=sk-...
-include .env
export

ARGS ?=

CARGO_PACKAGE := -p tickloop-agent
PACKAGE_TARGET ?= $(shell rustc -vV | sed -n 's/^host: //p')
PACKAGE_ARCHIVE = dist/tickloop-$(PACKAGE_TARGET).tar.gz
TICK_PACKAGES := longbridge futu

.PHONY: dev build install package site test check release release-site release-artifacts prototype-earnings-review package-ticks dev-ticks release-ticks

## Start in the current directory with the global configuration; Tick packages are installed separately with make dev-ticks; example: make dev ARGS="-c"
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
	tar -czf "$(PACKAGE_ARCHIVE)" -C "target/$(PACKAGE_TARGET)/release" tickloop tickhost tick-reader
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

## Release the Tick packages at the workspace version: push the tag ticks-v<version> on HEAD; GitHub Actions builds them on Apple Silicon, publishes them to tickloopai/ticks and verifies the downloads.
release-ticks:
	@scripts/release-ticks.sh tag

test:
	@cargo nextest --version >/dev/null 2>&1 || { echo 'make test requires cargo-nextest; install it with: cargo install --locked cargo-nextest' >&2; exit 1; }
	@mkdir -p /tmp/tickloop-test
	TICKLOOP_CONFIG_DIR=$$(mktemp -d /tmp/tickloop-test/config.XXXXXX) \
	TICKLOOP_TMPDIR=$$(mktemp -d /tmp/tickloop-test/tmp.XXXXXX) \
	NO_PROXY=127.0.0.1,localhost no_proxy=127.0.0.1,localhost \
	env $$(env | grep -o '^HERDR_[A-Za-z0-9_]*' | sed 's/^/-u /') \
	cargo nextest run --locked --workspace --no-fail-fast

check:
	cargo fmt --all --check
	cargo clippy --locked --workspace --all-targets -- -D warnings
	$(MAKE) test

## 发布已提交的二进制快照：标签取 Cargo.toml 的 workspace 版本，VERSION 只能与它相同
release:
	@node scripts/release.mjs binary

## 将官网快照的当前分支与标签一起推送；默认递增 patch，可用 VERSION=0.1.0 指定版本
release-site:
	@node scripts/release.mjs site

## 发布已提交的 Artifacts 快照；默认递增 patch，可用 VERSION=0.1.0 指定版本
release-artifacts:
	@node scripts/release.mjs artifacts

## 启动财报复盘原型；访问 http://127.0.0.1:8766/earnings-review.html?variant=A
prototype-earnings-review:
	python3 -m http.server 8766 --directory .scratch/earnings-review/prototype
